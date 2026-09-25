import { createHash, randomUUID } from "node:crypto";
import { isJsonValue, type JsonValue } from "@earendil-works/chord";
import Type from "typebox";
import { Check } from "typebox/value";
import type { HostModuleRequest, HostModuleStore, NativeHostApi, NativeHostModule } from "./rlm/host-module.ts";

export interface ReleaseGateModuleOptions {
	store: HostModuleStore;
	now?: () => number;
}

export type CheckOutcome = "passed" | "failed" | "missing";

type GateCheck = { name: string; required: boolean };

type Gate = {
	id: string;
	checks: GateCheck[];
	fixture_hash: string;
	/** SHA-256 of the canonical definition; every attempt records the digest it was judged against. */
	digest: string;
	defined_at: number;
	defined_by: string | null;
};

type RunRecord = { variant: string; fixture_hash: string; results: Record<string, CheckOutcome> };

type CheckChange = { check: string; required: boolean; baseline: CheckOutcome; candidate: CheckOutcome };

type Attempt = {
	id: string;
	gate_id: string;
	gate_digest: string;
	at: number;
	requester_task_id: string | null;
	baseline: RunRecord;
	candidate: RunRecord;
	decision: "passed" | "blocked";
	reasons: string[];
	regressions: CheckChange[];
	improvements: CheckChange[];
};

type GateDocument = { version: 1; gates: Gate[]; attempts: Attempt[] };

const MAX_CHECKS = 256;
const MAX_ATTEMPTS = 4096;
const DURABILITY_ERROR = "Release gate store durability is uncertain; reopen the owner";
const ID_PATTERN = /^[a-z][a-z0-9._-]{0,127}$/;
const CHECK_PATTERN = /^[A-Za-z0-9][A-Za-z0-9 ._:/-]{0,127}$/;
const HASH_PATTERN = /^[a-f0-9]{64}$/;
const VARIANT_PATTERN = /^\S.{0,255}$/;
const OUTCOMES: readonly CheckOutcome[] = ["passed", "failed", "missing"];

const outcomeSchema = Type.Union([Type.Literal("passed"), Type.Literal("failed"), Type.Literal("missing")]);
const gateSchema = Type.Object(
	{
		id: Type.String({ minLength: 1 }),
		checks: Type.Array(
			Type.Object(
				{ name: Type.String({ minLength: 1 }), required: Type.Boolean() },
				{ additionalProperties: false },
			),
		),
		fixture_hash: Type.String({ minLength: 1 }),
		digest: Type.String({ minLength: 1 }),
		defined_at: Type.Number(),
		defined_by: Type.Union([Type.String({ minLength: 1 }), Type.Null()]),
	},
	{ additionalProperties: false },
);
const runSchema = Type.Object(
	{
		variant: Type.String({ minLength: 1 }),
		fixture_hash: Type.String({ minLength: 1 }),
		results: Type.Record(Type.String(), outcomeSchema),
	},
	{ additionalProperties: false },
);
const changeSchema = Type.Object(
	{ check: Type.String(), required: Type.Boolean(), baseline: outcomeSchema, candidate: outcomeSchema },
	{ additionalProperties: false },
);
const attemptSchema = Type.Object(
	{
		id: Type.String({ minLength: 1 }),
		gate_id: Type.String({ minLength: 1 }),
		gate_digest: Type.String({ minLength: 1 }),
		at: Type.Number(),
		requester_task_id: Type.Union([Type.String({ minLength: 1 }), Type.Null()]),
		baseline: runSchema,
		candidate: runSchema,
		decision: Type.Union([Type.Literal("passed"), Type.Literal("blocked")]),
		reasons: Type.Array(Type.String()),
		regressions: Type.Array(changeSchema),
		improvements: Type.Array(changeSchema),
	},
	{ additionalProperties: false },
);
const documentSchema = Type.Object(
	{ version: Type.Literal(1), gates: Type.Array(gateSchema), attempts: Type.Array(attemptSchema) },
	{ additionalProperties: false },
);

function fields(payload: Record<string, unknown>, allowed: string[]): void {
	for (const key of Object.keys(payload)) {
		if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
	}
}

function plainObject(value: unknown, name: string): Record<string, unknown> {
	if (value === null || typeof value !== "object" || Array.isArray(value))
		throw new Error(`${name} must be a JSON object`);
	return value as Record<string, unknown>;
}

function gateIdField(value: unknown, name = "id"): string {
	if (typeof value !== "string" || !ID_PATTERN.test(value))
		throw new Error(`${name} must be a short lowercase identifier`);
	return value;
}

function hashField(value: unknown, name: string): string {
	if (typeof value !== "string" || !HASH_PATTERN.test(value))
		throw new Error(`${name} must be a lowercase SHA-256 hex string`);
	return value;
}

function checksField(value: unknown): GateCheck[] {
	if (!Array.isArray(value) || value.length === 0 || value.length > MAX_CHECKS)
		throw new Error(`checks must be a list of 1 to ${MAX_CHECKS} entries`);
	const seen = new Set<string>();
	const checks = value.map((item, index) => {
		const entry = plainObject(item, `checks[${index}]`);
		fields(entry, ["name", "required"]);
		if (typeof entry.name !== "string" || !CHECK_PATTERN.test(entry.name))
			throw new Error(`checks[${index}].name must be a check name of at most 128 characters`);
		if (typeof entry.required !== "boolean") throw new Error(`checks[${index}].required must be a boolean`);
		if (seen.has(entry.name)) throw new Error(`Duplicate check name: ${entry.name}`);
		seen.add(entry.name);
		return { name: entry.name, required: entry.required };
	});
	if (!checks.some((check) => check.required))
		throw new Error("A gate needs at least one required check; a gate with none could never block");
	return checks.sort((left, right) => (left.name < right.name ? -1 : left.name > right.name ? 1 : 0));
}

function gateDigest(id: string, checks: GateCheck[], fixtureHash: string): string {
	const canonical = JSON.stringify({
		id,
		checks: checks.map((check) => [check.name, check.required]),
		fixture_hash: fixtureHash,
	});
	return createHash("sha256").update(canonical).digest("hex");
}

function runField(value: unknown, name: string, gate: Gate): RunRecord {
	const run = plainObject(value, name);
	for (const key of Object.keys(run)) {
		if (!["variant", "fixture_hash", "results"].includes(key))
			throw new Error(
				`Unknown field ${name}.${key}: a run is {variant, fixture_hash, results: {check: "passed" | "failed" | "missing"}}`,
			);
	}
	if (typeof run.variant !== "string" || !VARIANT_PATTERN.test(run.variant))
		throw new Error(`${name}.variant must be a nonempty string of at most 256 characters`);
	const fixtureHash = hashField(run.fixture_hash, `${name}.fixture_hash`);
	const supplied = plainObject(run.results, `${name}.results`);
	const known = new Set(gate.checks.map((check) => check.name));
	for (const [check, outcome] of Object.entries(supplied)) {
		// The gate definition is the protected checker input: a run cannot introduce its own checks.
		if (!known.has(check)) throw new Error(`${name}.results names a check the gate does not define: ${check}`);
		if (!OUTCOMES.includes(outcome as CheckOutcome))
			throw new Error(`${name}.results.${check} must be passed, failed, or missing`);
	}
	const results: Record<string, CheckOutcome> = {};
	for (const check of gate.checks)
		results[check.name] = (supplied[check.name] as CheckOutcome | undefined) ?? "missing";
	return { variant: run.variant, fixture_hash: fixtureHash, results };
}

function judge(
	gate: Gate,
	baseline: RunRecord,
	candidate: RunRecord,
): Pick<Attempt, "decision" | "reasons" | "regressions" | "improvements"> {
	const reasons: string[] = [];
	let blocked = false;
	for (const [label, run] of [
		["baseline", baseline],
		["candidate", candidate],
	] as const) {
		if (run.fixture_hash !== gate.fixture_hash) {
			blocked = true;
			reasons.push(
				`Fixture mismatch: ${label} ${run.variant} ran against ${run.fixture_hash}, gate ${gate.id} requires ${gate.fixture_hash}`,
			);
		}
	}
	const regressions: CheckChange[] = [];
	const improvements: CheckChange[] = [];
	for (const check of gate.checks) {
		const before = baseline.results[check.name];
		const after = candidate.results[check.name];
		const change = { check: check.name, required: check.required, baseline: before, candidate: after };
		if (before === "passed" && after !== "passed") {
			regressions.push(change);
			if (check.required) {
				blocked = true;
				reasons.push(`Required check ${check.name} regressed: passed in baseline, ${after} in candidate`);
			} else {
				reasons.push(`Optional check ${check.name} regressed (${after}); reported, not blocking`);
			}
		} else if (before !== "passed" && after === "passed") {
			improvements.push(change);
		} else if (check.required && after !== "passed") {
			blocked = true;
			reasons.push(`Required check ${check.name} is ${after} in candidate`);
		}
	}
	if (!blocked) reasons.unshift(`Every required check passed in candidate ${candidate.variant}`);
	return { decision: blocked ? "blocked" : "passed", reasons, regressions, improvements };
}

/**
 * Release gates: frozen check definitions plus a recorded baseline-versus-candidate comparison.
 * A gate definition is immutable once defined and is the only source of which checks exist and
 * which are required. Every comparison attempt, blocked or passed, is recorded before it is
 * returned. This module only records and decides; it never prompts and nothing is forced to
 * consult it.
 */
export function createReleaseGateModule(options: ReleaseGateModuleOptions): NativeHostModule {
	let document: GateDocument = { version: 1, gates: [], attempts: [] };
	let loading: Promise<void> | undefined;
	let tail: Promise<void> = Promise.resolve();
	let broken = false;
	let clock: () => number = options.now ?? Date.now;

	async function write(next: GateDocument): Promise<void> {
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
		if (!isJsonValue(saved) || !Check(documentSchema, saved)) throw new Error("Invalid release gate document");
		const parsed = structuredClone(saved) as unknown as GateDocument;
		for (const gate of parsed.gates) {
			if (gate.digest !== gateDigest(gate.id, gate.checks, gate.fixture_hash))
				throw new Error(`Release gate ${gate.id} definition does not match its digest`);
		}
		document = parsed;
	}

	function enqueue<T>(change: () => Promise<T> | T): Promise<T> {
		const pending = tail.then(async () => {
			if (broken) throw new Error(DURABILITY_ERROR);
			loading ??= load();
			await loading;
			return change();
		});
		tail = pending.then(
			() => {},
			() => {},
		);
		return pending;
	}

	function findGate(id: string): Gate {
		const gate = document.gates.find((candidate) => candidate.id === id);
		if (!gate) throw new Error(`Unknown release gate: ${id}`);
		return gate;
	}

	async function define(request: HostModuleRequest, host: NativeHostApi): Promise<Gate> {
		const { payload } = request;
		fields(payload, ["id", "checks", "fixture_hash"]);
		const id = gateIdField(payload.id);
		const checks = checksField(payload.checks);
		const fixtureHash = hashField(payload.fixture_hash, "fixture_hash");
		const digest = gateDigest(id, checks, fixtureHash);
		const caller = host.callerTaskId(request.caller);
		return enqueue(async () => {
			const existing = document.gates.find((gate) => gate.id === id);
			if (existing) {
				if (existing.digest !== digest)
					throw new Error(`Release gate ${id} is already defined with different content and cannot be changed`);
				return structuredClone(existing);
			}
			const gate: Gate = { id, checks, fixture_hash: fixtureHash, digest, defined_at: clock(), defined_by: caller };
			await write({ ...document, gates: [...document.gates, gate] });
			return structuredClone(gate);
		});
	}

	async function compare(request: HostModuleRequest, host: NativeHostApi): Promise<Attempt> {
		const { payload } = request;
		fields(payload, ["gate_id", "baseline", "candidate"]);
		const gateId = gateIdField(payload.gate_id, "gate_id");
		const caller = host.callerTaskId(request.caller);
		return enqueue(async () => {
			if (document.attempts.length >= MAX_ATTEMPTS)
				throw new Error(`Release gate history is full (${MAX_ATTEMPTS} attempts)`);
			const gate = findGate(gateId);
			const baseline = runField(payload.baseline, "baseline", gate);
			const candidate = runField(payload.candidate, "candidate", gate);
			const attempt: Attempt = {
				id: `gate-attempt-${randomUUID()}`,
				gate_id: gate.id,
				gate_digest: gate.digest,
				at: clock(),
				requester_task_id: caller,
				baseline,
				candidate,
				...judge(gate, baseline, candidate),
			};
			await write({ ...document, attempts: [...document.attempts, attempt] });
			return structuredClone(attempt);
		});
	}

	async function history(request: HostModuleRequest): Promise<{ gate: Gate; attempts: Attempt[] }> {
		fields(request.payload, ["gate_id"]);
		const gateId = gateIdField(request.payload.gate_id, "gate_id");
		return enqueue(() =>
			structuredClone({
				gate: findGate(gateId),
				attempts: document.attempts.filter((attempt) => attempt.gate_id === gateId),
			}),
		);
	}

	async function list(request: HostModuleRequest): Promise<{ gates: Gate[] }> {
		fields(request.payload, []);
		return enqueue(() => structuredClone({ gates: document.gates }));
	}

	return {
		prefixes: ["gates."],
		async start(host) {
			if (options.now === undefined) clock = () => host.now();
			await enqueue(() => {});
		},
		async handle(request, host) {
			if (options.now === undefined) clock = () => host.now();
			switch (request.type) {
				case "gates.define":
					return define(request, host);
				case "gates.compare":
					return compare(request, host);
				case "gates.history":
					return history(request);
				case "gates.list":
					return list(request);
				default:
					throw new Error(`Unknown release gate request: ${request.type}`);
			}
		},
	};
}
