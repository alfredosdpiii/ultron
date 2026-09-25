import { afterEach, describe, expect, test } from "vitest";
import { checkGrant, createGrantModule } from "../src/ultron/grants.ts";
import { createMemoryModuleStore, type HostModuleStore } from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;
const REV_A = "a".repeat(40);
const REV_B = "b".repeat(40);

function memoryStore(): NativeHostStore {
	let value: unknown;
	return {
		read: async () => value as never,
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

function waitingLane() {
	return {
		findEntries: async () => [],
		getActiveTools: async () => [],
		setModel: async () => {},
		prompt: () => new Promise<never>(() => {}),
		abort: async () => ({ ok: true }),
		steer: async () => ({ ok: true, value: {} }),
	};
}

type Grant = { id: string; owner_task_id: string | null; expires_at: number; revoked_at: number | null };
type CheckResult = { allowed: boolean; reasons: string[]; enforced: boolean };

const hosts: NativeRlmHost[] = [];

afterEach(async () => {
	for (const host of hosts.splice(0)) await host.close();
});

async function setup(options: { store?: HostModuleStore; enforce?: boolean } = {}) {
	let time = 1_000_000;
	const clock = {
		now: () => time,
		advance: (ms: number) => {
			time += ms;
		},
	};
	const lane = waitingLane();
	const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, {
		store: memoryStore(),
		definitionStore: memoryStore(),
		deterministic: async ({ input }) => input,
		modules: [
			createGrantModule({
				store: options.store ?? createMemoryModuleStore(),
				now: clock.now,
				...(options.enforce === undefined ? {} : { enforce: options.enforce }),
			}),
		],
	});
	hosts.push(host);
	const call = <T = unknown>(type: string, payload: Record<string, unknown>, laneName?: string) =>
		host.handle(type, payload, context, laneName === undefined ? undefined : { lane: laneName }) as Promise<T>;
	const owner = await call<{ id: string }>("background.start", { prompt: "owner" });
	await new Promise((resolve) => setTimeout(resolve, 10));
	const ownerLane = `ultron.background-job.${owner.id}`;
	return { call, clock, owner: owner.id, ownerLane };
}

const binding = { scope: "repo:/work/app", revision: REV_A, policy: "apply-policy@3", action: "apply-patch" };

describe("grant module (A11)", () => {
	test("issues a grant owned by the calling lane and allows a matching check", async () => {
		const { call, owner, ownerLane } = await setup();
		const grant = await call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 }, ownerLane);
		expect(grant).toMatchObject({ ...binding, owner_task_id: owner, expires_at: 1_060_000, revoked_at: null });
		const result = await call<CheckResult>("grants.check", { id: grant.id, ...binding }, ownerLane);
		expect(result).toMatchObject({ allowed: true, reasons: [], enforced: false });
		const listed = await call<{ grants: Grant[]; enforced: boolean }>("grants.list", {}, ownerLane);
		expect(listed.grants.map((entry) => entry.id)).toEqual([grant.id]);
		expect(listed.enforced).toBe(false);
	});

	test("revocation denies later checks, and only the owner lineage may revoke", async () => {
		const { call, ownerLane } = await setup();
		const grant = await call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 }, ownerLane);
		const stranger = await call<{ id: string }>("background.start", { prompt: "stranger" });
		await new Promise((resolve) => setTimeout(resolve, 10));
		await expect(call("grants.revoke", { id: grant.id }, `ultron.background-job.${stranger.id}`)).rejects.toThrow(
			"owner or one of its ancestors",
		);
		// The root is an ancestor of every task.
		const revoked = await call<Grant>("grants.revoke", { id: grant.id });
		expect(revoked.revoked_at).toBe(1_000_000);
		const result = await call<CheckResult>("grants.check", { id: grant.id, ...binding }, ownerLane);
		expect(result.allowed).toBe(false);
		expect(result.reasons.join("\n")).toContain("revoked");
	});

	test("expiry is rechecked against the clock on every check", async () => {
		const { call, clock, ownerLane } = await setup();
		const grant = await call<Grant>("grants.issue", { ...binding, ttl_ms: 1_000 }, ownerLane);
		clock.advance(999);
		expect((await call<CheckResult>("grants.check", { id: grant.id, ...binding }, ownerLane)).allowed).toBe(true);
		clock.advance(1);
		const expired = await call<CheckResult>("grants.check", { id: grant.id, ...binding }, ownerLane);
		expect(expired.allowed).toBe(false);
		expect(expired.reasons.join("\n")).toContain("expired");
	});

	test("a changed base, policy, scope, or action is denied", async () => {
		const { call, ownerLane } = await setup();
		const grant = await call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 }, ownerLane);
		const check = (change: Record<string, string>) =>
			call<CheckResult>("grants.check", { id: grant.id, ...binding, ...change }, ownerLane);
		const changedBase = await check({ revision: REV_B });
		expect(changedBase).toMatchObject({ allowed: false });
		expect(changedBase.reasons).toEqual([expect.stringContaining("Base changed")]);
		const changedPolicy = await check({ policy: "apply-policy@4" });
		expect(changedPolicy.reasons).toEqual([expect.stringContaining("Policy changed")]);
		expect((await check({ scope: "repo:/work/other" })).reasons).toEqual([expect.stringContaining("Scope differs")]);
		expect((await check({ action: "push" })).reasons).toEqual([expect.stringContaining("Action differs")]);
		expect((await check({})).allowed).toBe(true);
		expect((await call<CheckResult>("grants.check", { id: "grant-nope", ...binding }, ownerLane)).reasons).toEqual([
			"Unknown grant",
		]);
	});

	test("descendants of the owner may use a grant; siblings and the root may not", async () => {
		const { call, owner, ownerLane } = await setup();
		const child = await call<{ id: string }>("background.start", { prompt: "child" }, ownerLane);
		const sibling = await call<{ id: string }>("background.start", { prompt: "sibling" });
		await new Promise((resolve) => setTimeout(resolve, 10));
		const grant = await call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 }, ownerLane);
		expect(grant.owner_task_id).toBe(owner);

		const fromChild = await call<CheckResult>(
			"grants.check",
			{ id: grant.id, ...binding },
			`ultron.background-job.${child.id}`,
		);
		expect(fromChild.allowed).toBe(true);
		const fromSibling = await call<CheckResult>(
			"grants.check",
			{ id: grant.id, ...binding },
			`ultron.background-job.${sibling.id}`,
		);
		expect(fromSibling.allowed).toBe(false);
		expect(fromSibling.reasons).toEqual([expect.stringContaining("Wrong owner")]);
		expect((await call<CheckResult>("grants.check", { id: grant.id, ...binding })).allowed).toBe(false);
		const siblingList = await call<{ grants: Grant[] }>("grants.list", {}, `ultron.background-job.${sibling.id}`);
		expect(siblingList.grants).toEqual([]);

		// A root-owned grant covers every task.
		const rootGrant = await call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 });
		expect(rootGrant.owner_task_id).toBeNull();
		expect(
			(
				await call<CheckResult>(
					"grants.check",
					{ id: rootGrant.id, ...binding },
					`ultron.background-job.${sibling.id}`,
				)
			).allowed,
		).toBe(true);
	});

	test("rejects a forged owner and other unknown or malformed fields", async () => {
		const { call, ownerLane } = await setup();
		await expect(
			call("grants.issue", { ...binding, ttl_ms: 60_000, owner: "someone-else" }, ownerLane),
		).rejects.toThrow("Unknown payload field: owner");
		await expect(
			call("grants.issue", { ...binding, ttl_ms: 60_000, owner_task_id: null }, ownerLane),
		).rejects.toThrow("Unknown payload field: owner_task_id");
		await expect(call("grants.issue", { ...binding, ttl_ms: 0 }, ownerLane)).rejects.toThrow("ttl_ms");
		await expect(call("grants.issue", { ...binding, policy: "no-version", ttl_ms: 5 }, ownerLane)).rejects.toThrow(
			"id@version",
		);
		await expect(call("grants.check", { id: "x", ...binding, owner: null }, ownerLane)).rejects.toThrow(
			"Unknown payload field: owner",
		);
		await expect(call("grants.list", { all: true }, ownerLane)).rejects.toThrow("Unknown payload field");
	});

	test("enforce defaults to false and is reported; opting in reports true", async () => {
		const off = await setup();
		const offGrant = await off.call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 }, off.ownerLane);
		const denied = await off.call<CheckResult>("grants.check", { id: offGrant.id, ...binding, revision: REV_B });
		expect(denied).toMatchObject({ allowed: false, enforced: false });

		const on = await setup({ enforce: true });
		const onGrant = await on.call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 }, on.ownerLane);
		expect(await on.call<CheckResult>("grants.check", { id: onGrant.id, ...binding }, on.ownerLane)).toMatchObject({
			allowed: true,
			enforced: true,
		});
	});

	test("grants and revocations survive a new module instance", async () => {
		const store = createMemoryModuleStore();
		const first = await setup({ store });
		const kept = await first.call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 });
		const dropped = await first.call<Grant>("grants.issue", { ...binding, ttl_ms: 60_000 });
		await first.call("grants.revoke", { id: dropped.id });

		const second = await setup({ store });
		const listed = await second.call<{ grants: Grant[] }>("grants.list", {});
		expect(listed.grants.map((grant) => [grant.id, grant.revoked_at !== null])).toEqual([
			[kept.id, false],
			[dropped.id, true],
		]);
		expect((await second.call<CheckResult>("grants.check", { id: kept.id, ...binding })).allowed).toBe(true);
		expect((await second.call<CheckResult>("grants.check", { id: dropped.id, ...binding })).allowed).toBe(false);
	});

	test("checkGrant is a pure evaluation that reports every failing binding", () => {
		const document = {
			version: 1 as const,
			grants: [
				{
					id: "g1",
					...binding,
					owner_task_id: "t1",
					issued_at: 0,
					expires_at: 100,
					revoked_at: 50,
					revoked_by: "root",
				},
			],
		};
		const result = checkGrant(document, {
			id: "g1",
			...binding,
			revision: REV_B,
			callerTaskId: "t2",
			now: 200,
			tasks: [{ id: "t1" }, { id: "t2" }],
		});
		expect(result.allowed).toBe(false);
		expect(result.reasons).toHaveLength(4);
		expect(
			checkGrant(
				{ version: 1, grants: [{ ...document.grants[0], revoked_at: null }] },
				{ id: "g1", ...binding, callerTaskId: "t3", now: 10, tasks: [{ id: "t1" }, { id: "t3", parentId: "t1" }] },
			).allowed,
		).toBe(true);
	});
});
