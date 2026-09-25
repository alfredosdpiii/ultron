import { randomUUID } from "node:crypto";
import { isJsonValue, type JsonValue } from "@ultron/chord";
import Type from "typebox";
import { Check } from "typebox/value";
import { readVersioned } from "./format-version.ts";
import type { HostModuleRequest, HostModuleStore, NativeHostApi, NativeHostModule } from "./rlm/host-module.ts";

export interface GrantModuleOptions {
	store: HostModuleStore;
	now?: () => number;
	/**
	 * Reported as `enforced` on every `grants.check` result. Defaults to false.
	 *
	 * With the default, grants are advisory only: `grants.check` still returns the true
	 * evaluation, but nothing in Ultron consults grants before acting, so no tool, delegation,
	 * or patch is ever blocked and no permission prompt is ever shown. A stricter profile that
	 * wants approvals must both pass `enforce: true` and make its own gated action call
	 * {@link checkGrant} (or `grants.check`) and refuse when `allowed` is false.
	 */
	enforce?: boolean;
}

export type Grant = {
	id: string;
	/** For example `repo:/path` or `task:<id>`. */
	scope: string;
	/** Base the approval was made against, such as a git commit or content SHA-256. */
	revision: string;
	/** `id@version` of the policy text the grant was issued under. */
	policy: string;
	action: string;
	/** Taken from the issuing lane, never from the payload. Null is the root agent. */
	owner_task_id: string | null;
	issued_at: number;
	expires_at: number;
	revoked_at: number | null;
	revoked_by: string | null;
};

export type GrantDocument = { version: 1; grants: Grant[] };

export interface GrantCheckRequest {
	id: string;
	scope: string;
	revision: string;
	policy: string;
	action: string;
	/** Task that owns the calling lane, or null for the root agent. */
	callerTaskId: string | null;
	now: number;
	/** Task tree used to decide whether the caller descends from the grant owner. */
	tasks: readonly { id: string; parentId?: string }[];
}

export interface GrantCheckResult {
	grant_id: string;
	allowed: boolean;
	/** Every reason the grant does not authorize this request; empty when allowed. */
	reasons: string[];
	checked_at: number;
}

const MAX_TTL_MS = 30 * 24 * 60 * 60 * 1000;
const MAX_GRANTS = 4096;
const DURABILITY_ERROR = "Grant store durability is uncertain; reopen the owner";
const SCOPE_PATTERN = /^[a-z][a-z0-9_-]{0,31}:\S.{0,1023}$/;
const REVISION_PATTERN = /^[A-Za-z0-9._:/@+-]{1,256}$/;
const POLICY_PATTERN = /^[a-z][a-z0-9._-]{0,127}@[A-Za-z0-9._-]{1,64}$/;
const ACTION_PATTERN = /^[a-z][a-z0-9_.-]{0,63}$/;

const grantSchema = Type.Object(
	{
		id: Type.String({ minLength: 1 }),
		scope: Type.String({ minLength: 1 }),
		revision: Type.String({ minLength: 1 }),
		policy: Type.String({ minLength: 1 }),
		action: Type.String({ minLength: 1 }),
		owner_task_id: Type.Union([Type.String({ minLength: 1 }), Type.Null()]),
		issued_at: Type.Number(),
		expires_at: Type.Number(),
		revoked_at: Type.Union([Type.Number(), Type.Null()]),
		revoked_by: Type.Union([Type.String({ minLength: 1 }), Type.Null()]),
	},
	{ additionalProperties: false },
);
const documentSchema = Type.Object(
	{ version: Type.Literal(1), grants: Type.Array(grantSchema) },
	{ additionalProperties: false },
);

function fields(payload: Record<string, unknown>, allowed: string[]): void {
	for (const key of Object.keys(payload)) {
		if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
	}
}

function matching(value: unknown, name: string, pattern: RegExp, description: string): string {
	if (typeof value !== "string" || !pattern.test(value)) throw new Error(`${name} must be ${description}`);
	return value;
}

const scopeField = (value: unknown) => matching(value, "scope", SCOPE_PATTERN, "kind:target, such as repo:/path");
const revisionField = (value: unknown) =>
	matching(value, "revision", REVISION_PATTERN, "a commit, content hash, or other base identifier");
const policyField = (value: unknown) => matching(value, "policy", POLICY_PATTERN, "id@version");
const actionField = (value: unknown) => matching(value, "action", ACTION_PATTERN, "a short lowercase identifier");

function idField(value: unknown): string {
	if (typeof value !== "string" || !value.trim()) throw new Error("id must be a nonempty string");
	return value;
}

/** True when `taskId` is `ancestorId` or descends from it. A null ancestor is the root, which every task descends from. */
function descendsFrom(
	taskId: string | null,
	ancestorId: string | null,
	tasks: readonly { id: string; parentId?: string }[],
): boolean {
	if (ancestorId === null) return true;
	if (taskId === null) return false;
	const parents = new Map(tasks.map((task) => [task.id, task.parentId]));
	const seen = new Set<string>();
	for (let current: string | undefined = taskId; current !== undefined; current = parents.get(current)) {
		if (current === ancestorId) return true;
		if (seen.has(current)) return false;
		seen.add(current);
	}
	return false;
}

/**
 * Pure grant evaluation. Nothing is cached: every call re-derives the answer from the stored
 * grant, the current clock, and the current task tree. A grant authorizes the request only
 * when it exists, is not revoked or expired, binds exactly the requested scope, revision,
 * policy, and action, and the caller is the owner or one of the owner's descendants.
 */
export function checkGrant(document: GrantDocument, request: GrantCheckRequest): GrantCheckResult {
	const base = { grant_id: request.id, checked_at: request.now };
	const grant = document.grants.find((candidate) => candidate.id === request.id);
	if (!grant) return { ...base, allowed: false, reasons: ["Unknown grant"] };
	const reasons: string[] = [];
	if (grant.revoked_at !== null) reasons.push(`Grant was revoked at ${grant.revoked_at}`);
	if (request.now >= grant.expires_at) reasons.push(`Grant expired at ${grant.expires_at}`);
	if (grant.revision !== request.revision)
		reasons.push(`Base changed: grant was approved against ${grant.revision}, request is for ${request.revision}`);
	if (grant.policy !== request.policy)
		reasons.push(`Policy changed: grant was issued under ${grant.policy}, request is under ${request.policy}`);
	if (grant.scope !== request.scope)
		reasons.push(`Scope differs: grant covers ${grant.scope}, request is for ${request.scope}`);
	if (grant.action !== request.action)
		reasons.push(`Action differs: grant covers ${grant.action}, request is for ${request.action}`);
	if (!descendsFrom(request.callerTaskId, grant.owner_task_id, request.tasks))
		reasons.push(
			`Wrong owner: grant belongs to ${grant.owner_task_id ?? "the root agent"}; caller ${request.callerTaskId ?? "root"} is neither the owner nor its descendant`,
		);
	return { ...base, allowed: reasons.length === 0, reasons };
}

/**
 * Scoped approvals bound to scope, revision, policy, action, owner, and expiry, re-evaluated on
 * every check. Dormant by default: see {@link GrantModuleOptions.enforce}. This module never
 * prompts; issuing a grant is an explicit request, not a confirmation dialog.
 */
export function createGrantModule(options: GrantModuleOptions): NativeHostModule {
	const enforced = options.enforce ?? false;
	let document: GrantDocument = { version: 1, grants: [] };
	let loading: Promise<void> | undefined;
	let tail: Promise<void> = Promise.resolve();
	let broken = false;
	let clock: () => number = options.now ?? Date.now;

	async function write(next: GrantDocument): Promise<void> {
		try {
			await options.store.write(structuredClone(next) as unknown as JsonValue);
		} catch {
			broken = true;
			throw new Error(DURABILITY_ERROR);
		}
		document = next;
	}

	async function load(): Promise<void> {
		const stored = await options.store.read();
		if (stored === undefined) return;
		const saved = isJsonValue(stored) ? readVersioned("ultron.module/grants", stored) : stored;
		if (!isJsonValue(saved) || !Check(documentSchema, saved)) throw new Error("Invalid grant document");
		document = structuredClone(saved) as unknown as GrantDocument;
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

	async function issue(request: HostModuleRequest, host: NativeHostApi): Promise<Grant> {
		const { payload } = request;
		fields(payload, ["scope", "revision", "policy", "action", "ttl_ms"]);
		const scope = scopeField(payload.scope);
		const revision = revisionField(payload.revision);
		const policy = policyField(payload.policy);
		const action = actionField(payload.action);
		const ttl = payload.ttl_ms;
		if (typeof ttl !== "number" || !Number.isSafeInteger(ttl) || ttl < 1 || ttl > MAX_TTL_MS)
			throw new Error(`ttl_ms must be an integer from 1 to ${MAX_TTL_MS}`);
		const owner = host.callerTaskId(request.caller);
		return enqueue(async () => {
			if (document.grants.length >= MAX_GRANTS) throw new Error(`Grant store is full (${MAX_GRANTS})`);
			const issuedAt = clock();
			const grant: Grant = {
				id: `grant-${randomUUID()}`,
				scope,
				revision,
				policy,
				action,
				owner_task_id: owner,
				issued_at: issuedAt,
				expires_at: issuedAt + ttl,
				revoked_at: null,
				revoked_by: null,
			};
			await write({ ...document, grants: [...document.grants, grant] });
			return structuredClone(grant);
		});
	}

	async function revoke(request: HostModuleRequest, host: NativeHostApi): Promise<Grant> {
		fields(request.payload, ["id"]);
		const id = idField(request.payload.id);
		const caller = host.callerTaskId(request.caller);
		const tasks = await host.tasks();
		return enqueue(async () => {
			const grant = document.grants.find((candidate) => candidate.id === id);
			if (!grant) throw new Error("Unknown grant");
			// The owner or any ancestor of the owner may withdraw an approval.
			if (!descendsFrom(grant.owner_task_id, caller, tasks))
				throw new Error("Only the grant owner or one of its ancestors may revoke it");
			if (grant.revoked_at !== null) return structuredClone(grant);
			const revoked: Grant = { ...grant, revoked_at: clock(), revoked_by: caller ?? "root" };
			await write({
				...document,
				grants: document.grants.map((candidate) => (candidate.id === id ? revoked : candidate)),
			});
			return structuredClone(revoked);
		});
	}

	async function list(
		request: HostModuleRequest,
		host: NativeHostApi,
	): Promise<{ grants: Grant[]; enforced: boolean }> {
		fields(request.payload, []);
		const caller = host.callerTaskId(request.caller);
		const tasks = await host.tasks();
		return enqueue(() => ({
			// Grants the caller may use (owned by it or an ancestor) or may revoke (owned by a descendant).
			grants: structuredClone(
				document.grants.filter(
					(grant) =>
						descendsFrom(caller, grant.owner_task_id, tasks) || descendsFrom(grant.owner_task_id, caller, tasks),
				),
			),
			enforced,
		}));
	}

	async function check(
		request: HostModuleRequest,
		host: NativeHostApi,
	): Promise<GrantCheckResult & { enforced: boolean }> {
		const { payload } = request;
		fields(payload, ["id", "scope", "revision", "policy", "action"]);
		const id = idField(payload.id);
		const scope = scopeField(payload.scope);
		const revision = revisionField(payload.revision);
		const policy = policyField(payload.policy);
		const action = actionField(payload.action);
		const callerTaskId = host.callerTaskId(request.caller);
		const tasks = await host.tasks();
		return enqueue(() => ({
			...checkGrant(document, { id, scope, revision, policy, action, callerTaskId, now: clock(), tasks }),
			enforced,
		}));
	}

	return {
		prefixes: ["grants."],
		async start(host) {
			if (options.now === undefined) clock = () => host.now();
			await enqueue(() => {});
		},
		async handle(request, host) {
			if (options.now === undefined) clock = () => host.now();
			switch (request.type) {
				case "grants.issue":
					return issue(request, host);
				case "grants.revoke":
					return revoke(request, host);
				case "grants.list":
					return list(request, host);
				case "grants.check":
					return check(request, host);
				default:
					throw new Error(`Unknown grants request: ${request.type}`);
			}
		},
	};
}
