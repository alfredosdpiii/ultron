import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import type { JsonValue } from "@earendil-works/chord";
import { afterEach, describe, expect, test } from "vitest";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";

const context = {} as never;

/** One durable document with owner fencing; `crashAt` kills the owner on its Nth write. */
function durable() {
	let document: JsonValue | undefined;
	let generation = 0;
	return {
		get document() {
			return structuredClone(document);
		},
		open(options: { crashOnWrite?: (next: JsonValue, index: number) => boolean } = {}) {
			const mine = ++generation;
			let writes = 0;
			let dead = false;
			return {
				read: async () => structuredClone(document),
				write: async (next: JsonValue) => {
					writes += 1;
					if (!dead && options.crashOnWrite?.(next, writes)) dead = true;
					if (dead || mine !== generation) throw new Error("owner is gone");
					document = structuredClone(next);
				},
			};
		},
	};
}

type Mode = "respond" | "lose-response";

/** A disposable payment-like service. It records every request and applies each idempotency key once. */
async function fakeService() {
	const requests: Array<{ key: string; amount: number }> = [];
	const effects = new Map<string, { charge: string; amount: number }>();
	const held: ServerResponse[] = [];
	let mode: Mode = "respond";
	const server: Server = createServer((request: IncomingMessage, response: ServerResponse) => {
		let body = "";
		request.on("data", (chunk) => {
			body += chunk;
		});
		request.on("end", () => {
			const key = String(request.headers["idempotency-key"]);
			const { amount } = JSON.parse(body) as { amount: number };
			requests.push({ key, amount });
			let effect = effects.get(key);
			if (!effect) {
				effect = { charge: `charge-${effects.size + 1}`, amount };
				effects.set(key, effect);
			}
			// The effect is committed; in lose-response mode the reply never reaches the caller.
			if (mode === "lose-response") {
				held.push(response);
				return;
			}
			response.setHeader("content-type", "application/json");
			response.end(JSON.stringify(effect));
		});
	});
	await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
	const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/charges`;
	return {
		url,
		requests,
		effects,
		setMode(next: Mode) {
			mode = next;
		},
		async close() {
			for (const response of held) response.destroy();
			server.closeAllConnections();
			await new Promise<void>((resolve) => server.close(() => resolve()));
		},
	};
}

const definition = {
	id: "charge-card",
	version: "1",
	strategy: "deterministic",
	instructions: "Charge the card once using the supplied idempotency key.",
	inputSchema: {
		type: "object",
		properties: { amount: { type: "integer" }, idempotency_key: { type: "string" } },
		required: ["amount", "idempotency_key"],
		additionalProperties: false,
	},
	outputSchema: {
		type: "object",
		properties: { charge: { type: "string" }, amount: { type: "integer" } },
		required: ["charge", "amount"],
	},
	maxRepairs: 0,
	inputDescription: "{amount, idempotency_key}",
	outputDescription: "{charge, amount}",
};

type Stores = {
	tasks: ReturnType<typeof durable>;
	usage: ReturnType<typeof durable>;
	definitions: ReturnType<typeof durable>;
};

const cleanups: Array<() => Promise<void>> = [];
afterEach(async () => {
	for (const cleanup of cleanups.splice(0).reverse()) await cleanup().catch(() => {});
});

async function setup() {
	const service = await fakeService();
	cleanups.push(() => service.close());
	const stores: Stores = { tasks: durable(), usage: durable(), definitions: durable() };
	let adapterCalls = 0;
	const owner = (taskOptions: Parameters<ReturnType<typeof durable>["open"]>[0] = {}) => {
		const host = new NativeRlmHost({ lane: async () => ({}) } as never, {} as never, {
			store: stores.tasks.open(taskOptions),
			definitionStore: stores.definitions.open(),
			usage: new NativeUsageLedger(stores.usage.open()),
			deterministic: async ({ input, signal }) => {
				adapterCalls += 1;
				const { amount, idempotency_key } = input as { amount: number; idempotency_key: string };
				const response = await fetch(service.url, {
					method: "POST",
					headers: { "content-type": "application/json", "idempotency-key": idempotency_key },
					body: JSON.stringify({ amount }),
					signal,
				});
				return response.json();
			},
		});
		cleanups.push(() => host.close());
		return <T = Record<string, unknown>>(type: string, payload: Record<string, unknown> = {}) =>
			host.handle(type, payload, context) as Promise<T>;
	};
	return {
		service,
		stores,
		owner,
		get adapterCalls() {
			return adapterCalls;
		},
	};
}

const charge = (key: string, amount = 500) => ({
	definition: "charge-card@1",
	input: { amount, idempotency_key: key },
	key: `task:${key}`,
});

async function until(condition: () => boolean) {
	for (let attempt = 0; attempt < 200 && !condition(); attempt += 1)
		await new Promise((resolve) => setTimeout(resolve, 5));
	expect(condition()).toBe(true);
}

describe("A21 replay does not duplicate external effects; uncertainty stays explicit", () => {
	test("a lost response followed by an owner crash is not replayed, and the same key returns the durable interrupted result", async () => {
		const fixture = await setup();
		const first = fixture.owner();
		await first("agents.register", { definition });
		fixture.service.setMode("lose-response");
		const spawned = await first<{ id: string }>("agents.spawn", charge("order-1"));
		await until(() => fixture.service.requests.length === 1);
		expect(fixture.stores.tasks.document).toMatchObject({ tasks: [{ id: spawned.id, state: "running" }] });

		// Owner crash: the effect happened, its response never arrived, and nothing terminal was committed.
		fixture.service.setMode("respond");
		const restarted = fixture.owner();
		const inspected = await restarted("agents.inspect", { id: spawned.id });
		expect(inspected).toMatchObject({
			state: "interrupted",
			result: {
				status: "interrupted",
				error: "Owner ended; automatic replay is disabled",
				verification: "unverified",
			},
		});
		// Restart alone replays nothing.
		expect(fixture.service.requests).toHaveLength(1);

		// Re-invoking with the same key is a lookup of the durable outcome, not a second attempt.
		const again = await restarted("agents.invoke", charge("order-1"));
		expect(again).toEqual({
			status: "interrupted",
			error: "Owner ended; automatic replay is disabled",
			verification: "unverified",
		});
		expect(await restarted("agents.result", { id: spawned.id })).toEqual(again);
		await expect(restarted("agents.invoke", charge("order-1", 900))).rejects.toThrow("Idempotency key reused");
		expect(fixture.service.requests).toHaveLength(1);
		expect(fixture.service.effects.size).toBe(1);
		expect(fixture.adapterCalls).toBe(1);

		// The uncertainty is explicit in accounting too: the ended owner's reservation settles as unknown.
		const status = await restarted<{ usage: { activeReservations: number; usage: { taskCalls: number } } }>(
			"agents.status",
		);
		expect(status.usage.activeReservations).toBe(0);
		const ledger = fixture.stores.usage.document as {
			roots: Record<string, { calls: Array<{ kind: string; requestKey: string; status: string }> }>;
		};
		expect(Object.values(ledger.roots)[0]!.calls).toMatchObject([
			{ kind: "task", requestKey: "task:order-1", status: "unknown" },
		]);
	});

	test("crashes before launch, after the effect's response, and after commit never duplicate the effect", async () => {
		const fixture = await setup();
		const setupOwner = fixture.owner();
		await setupOwner("agents.register", { definition });

		// Boundary 1: admission committed, owner dies before the task is marked running.
		const beforeLaunch = fixture.owner({
			crashOnWrite: (next) =>
				(next as { tasks: Array<{ state: string }> }).tasks.some((task) => task.state === "running"),
		});
		await beforeLaunch("agents.spawn", charge("order-2"));
		await new Promise((resolve) => setTimeout(resolve, 20));
		expect(fixture.service.requests).toHaveLength(0);
		expect(fixture.adapterCalls).toBe(0);

		// Boundary 2: the service responded, but the owner died before committing the terminal result.
		const beforeCommit = fixture.owner({
			crashOnWrite: (next) =>
				(next as { tasks: Array<{ key: string; state: string }> }).tasks.some(
					(task) => task.key === "task:order-3" && task.state === "completed",
				),
		});
		const lost = await beforeCommit<{ status: string }>("agents.invoke", charge("order-3")).catch((error) => ({
			status: `rejected: ${String(error)}`,
		}));
		expect(lost.status).not.toBe("succeeded");
		expect(fixture.service.requests.map((request) => request.key)).toEqual(["order-3"]);

		// Boundary 3: everything committed, but the caller lost the response and retries.
		const committed = fixture.owner();
		const success = await committed("agents.invoke", charge("order-4"));
		expect(success).toMatchObject({ status: "succeeded", value: { charge: "charge-2", amount: 500 } });

		const restarted = fixture.owner();
		expect(await restarted("agents.invoke", charge("order-2"))).toMatchObject({ status: "interrupted" });
		expect(await restarted("agents.invoke", charge("order-3"))).toMatchObject({ status: "interrupted" });
		expect(await restarted("agents.invoke", charge("order-4"))).toEqual(success);
		expect(fixture.service.requests.map((request) => request.key)).toEqual(["order-3", "order-4"]);
		expect(fixture.adapterCalls).toBe(2);

		// Retrying an uncertain effect is an explicit new request; the service's own key still dedupes it.
		const explicitRetry = await restarted("agents.invoke", { ...charge("order-3"), key: "task:order-3:retry-1" });
		expect(explicitRetry).toMatchObject({ status: "succeeded", value: { charge: "charge-1", amount: 500 } });
		expect(fixture.service.effects.size).toBe(2);
		const tasks = (await restarted<{ tasks: Array<{ state: string }> }>("agents.status")).tasks;
		expect(tasks.map((task) => task.state)).toEqual(["interrupted", "interrupted", "completed", "completed"]);
	});
});
