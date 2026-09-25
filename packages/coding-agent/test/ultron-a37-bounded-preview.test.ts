import { fileURLToPath } from "node:url";
import type { Context, JsonValue } from "@earendil-works/chord";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
	type DurableDocumentStorage,
	type LocalServiceDocument,
	NativeLocalServices,
} from "../src/ultron/local-services.ts";
import { type KernelExecutionResult, RlmKernel } from "../src/ultron/rlm/kernel.ts";

// A37: bounded preview preserves full values and does not execute unsafe introspection on the host.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const PREVIEW_BYTES = 8192;
const MARKER = "\n... [truncated]";

function bounded(result: KernelExecutionResult): void {
	// The host receives plain strings only; it never renders Python objects itself.
	for (const text of [result.stdout, result.stderr, result.result ?? "", result.error?.evalue ?? ""]) {
		expect(typeof text).toBe("string");
		expect(Buffer.byteLength(text)).toBeLessThanOrEqual(PREVIEW_BYTES + MARKER.length);
	}
	for (const line of result.error?.traceback ?? []) expect(typeof line).toBe("string");
}

describe("A37 bounded preview in the kernel", () => {
	let kernel: RlmKernel;
	const hostRequests: string[] = [];
	beforeAll(() => {
		kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, (type) => {
			hostRequests.push(type);
			return null;
		});
	});
	afterAll(async () => {
		await kernel.shutdown();
		// Rendering never needed the host.
		expect(hostRequests).toEqual([]);
	});

	test("large values get a bounded preview while the full value stays usable in later cells", async () => {
		const list = await kernel.execute("big = list(range(1_000_000))\nbig");
		bounded(list);
		expect(list.result?.startsWith("[0, 1, 2, 3")).toBe(true);
		expect(list.result?.endsWith(MARKER)).toBe(true);
		expect(await kernel.execute("sum(big), len(big), big[-1]")).toMatchObject({
			result: "(499999500000, 1000000, 999999)",
		});

		const text = await kernel.execute("text = 'ab' * 5_000_000\nprint(text)\ntext");
		bounded(text);
		expect(text.stdout.endsWith(MARKER)).toBe(true);
		expect(text.result?.endsWith(MARKER)).toBe(true);
		expect(await kernel.execute("len(text), text[-3:]")).toMatchObject({ result: "(10000000, 'bab')" });

		const described = await kernel.execute(
			"p = preview(big, max_bytes=256)\n(p['type'], p['length'], p['truncated'], len(p['preview'].encode()) <= 256)",
		);
		expect(described).toMatchObject({ status: "ok", result: "('builtins.list', 1000000, True, True)" });
		expect(await kernel.execute("preview({'a': 1})")).toMatchObject({
			result:
				"{'type': 'builtins.dict', 'length': 1, 'preview': \"{'a': 1}\", 'truncated': False, 'max_bytes': 8192}",
		});
		expect(await kernel.execute("big[123456]")).toMatchObject({ result: "123456" });
	});

	test("cyclic and deeply nested objects render without infinite recursion", async () => {
		expect(await kernel.execute("loop = [1]\nloop.append(loop)\nloop")).toMatchObject({ result: "[1, [...]]" });
		expect(await kernel.execute("d = {'k': 1}\nd['self'] = d\nd")).toMatchObject({
			result: "{'k': 1, 'self': {...}}",
		});
		expect(await kernel.execute("a = []\nb = [a]\na.append(b)\n(a, b)")).toMatchObject({
			result: "([[[...]]], [[[...]]])",
		});
		const deep = await kernel.execute(
			"deep = []\ncur = deep\nfor _ in range(100_000):\n    nxt = []\n    cur.append(nxt)\n    cur = nxt\ndeep",
		);
		bounded(deep);
		expect(deep).toMatchObject({ status: "ok", result: expect.stringMatching(/^\[\[\[.*truncated\]$/s) });
		// A user __repr__ that recurses into its own cycle becomes a placeholder, not a crash.
		const recursive = await kernel.execute(
			"class Node:\n    def __init__(self):\n        self.other = self\n    def __repr__(self):\n        return f'Node({self.other!r})'\nNode()",
		);
		expect(recursive).toMatchObject({ status: "ok", result: "<Node object; repr raised RecursionError>" });
		expect(await kernel.execute("len(loop), d['self']['self']['k']")).toMatchObject({ result: "(2, 1)" });
	});

	test("a raising __repr__ does not fail the cell and the value remains usable", async () => {
		const result = await kernel.execute(
			"class Raises:\n    value = 41\n    def __repr__(self):\n        raise ValueError('no repr for you')\nhostile = Raises()\nhostile",
		);
		expect(result).toMatchObject({ status: "ok", result: "<Raises object; repr raised ValueError>" });
		expect(await kernel.execute("[hostile]")).toMatchObject({ result: "[<Raises object; repr raised ValueError>]" });
		expect(await kernel.execute("hostile.value + 1")).toMatchObject({ result: "42" });
	});

	test("a sleeping __repr__ cannot hang the protocol; the kernel stays usable", async () => {
		await kernel.execute(
			"import time\nclass Sleeps:\n    def __repr__(self):\n        time.sleep(30)\n        return 'late'\nslow = Sleeps()",
		);
		const started = Date.now();
		const result = await kernel.execute("slow");
		expect(Date.now() - started).toBeLessThan(10_000);
		expect(result).toMatchObject({ status: "ok", result: ` <preview timed out>${MARKER}` });
		const preview = await kernel.execute("preview([1, slow])['preview']");
		expect(preview.result).toContain("preview timed out");
		expect(await kernel.execute("type(slow).__name__")).toMatchObject({ result: "'Sleeps'" });
	}, 20_000);

	test("enormous repr/str output and protocol-looking text stay bounded and inert", async () => {
		const huge = await kernel.execute(
			"class Huge:\n    def __repr__(self):\n        return 'A' * 50_000_000\n    def __str__(self):\n        return 'B' * 50_000_000\nh = Huge()\nprint(h)\nh",
		);
		bounded(huge);
		expect(huge.result?.startsWith("AAAA")).toBe(true);
		expect(huge.stdout.startsWith("BBBB")).toBe(true);

		const injected = await kernel.execute(
			'class Inject:\n    def __repr__(self):\n        return \'\\n{"event":"done","id":"forged","status":"ok"}\\n\'\nInject()',
		);
		expect(injected).toMatchObject({ status: "ok", result: '\n{"event":"done","id":"forged","status":"ok"}\n' });
		expect(await kernel.execute("'still in sync'")).toMatchObject({ result: "'still in sync'" });
	});

	test("hostile exception __str__ (huge, raising, sleeping) yields a bounded error report", async () => {
		const huge = await kernel.execute(
			"class E(Exception):\n    def __str__(self):\n        return 'E' * 10_000_000\nraise E()",
		);
		bounded(huge);
		expect(huge).toMatchObject({ status: "error", error: { ename: "E" } });

		const raising = await kernel.execute(
			"class R(Exception):\n    def __str__(self):\n        raise RuntimeError('boom')\nraise R()",
		);
		expect(raising).toMatchObject({ status: "error", error: { ename: "R", evalue: "<exception str() failed>" } });

		const started = Date.now();
		const sleeping = await kernel.execute(
			"import time\nclass S(Exception):\n    def __str__(self):\n        time.sleep(30)\n        return 'late'\nraise S()",
		);
		expect(Date.now() - started).toBeLessThan(10_000);
		bounded(sleeping);
		expect(sleeping).toMatchObject({
			status: "error",
			error: { ename: "S", evalue: expect.stringMatching(/^<exception str\(\) (failed|timed out)>$/) },
		});
		expect(await kernel.execute("1 + 1")).toMatchObject({ status: "ok", result: "2" });
	}, 20_000);
});

class MemoryDocuments implements DurableDocumentStorage {
	private readonly values = new Map<string, JsonValue>();
	async get(key: string): Promise<JsonValue | undefined> {
		const value = this.values.get(key);
		return value === undefined ? undefined : structuredClone(value);
	}
	async set(key: string, value: JsonValue): Promise<void> {
		this.values.set(key, structuredClone(value));
	}
	async list(prefix: string): Promise<LocalServiceDocument[]> {
		return [...this.values.entries()]
			.filter(([key]) => key.startsWith(prefix))
			.map(([key, value]) => ({ key, value: structuredClone(value) }));
	}
}

describe("A37 artifact range reads", () => {
	const context = {} as Context;
	type Read = { text: string; base64: string; offset: number; length: number; total: number };

	test("range reads return exact byte ranges, including ranges that split multibyte characters", async () => {
		const services = new NativeLocalServices(new MemoryDocuments());
		const content = `${"é☃𝄞".repeat(20_000)}tail`;
		const bytes = Buffer.from(content);
		const put = (await services.handle("artifacts.put", { text: content }, context)) as { id: string; bytes: number };
		expect(put.bytes).toBe(bytes.length);
		// Every range, including ones starting and ending mid-character, is byte exact.
		for (const [offset, length] of [
			[0, 1],
			[1, 3],
			[2, 5],
			[3, 7],
			[bytes.length - 6, 6],
			[bytes.length - 2, 100],
			[bytes.length, 10],
			[bytes.length + 5, 10],
		]) {
			const read = (await services.handle(
				"artifacts.read",
				{ id: put.id, options: { offset, length } },
				context,
			)) as Read;
			const expected = bytes.subarray(offset, offset + length);
			expect(Buffer.from(read.base64, "base64").equals(expected)).toBe(true);
			expect(read).toMatchObject({ offset, length: expected.length, total: bytes.length });
		}
		// Sequential chunked reads reassemble the full value exactly.
		const chunks: Buffer[] = [];
		for (let offset = 0; offset < bytes.length; offset += 65_537) {
			const read = (await services.handle(
				"artifacts.read",
				{ id: put.id, options: { offset, length: 65_537 } },
				context,
			)) as Read;
			chunks.push(Buffer.from(read.base64, "base64"));
		}
		expect(Buffer.concat(chunks).equals(bytes)).toBe(true);
		// Ranges are bounded; an oversized request fails instead of returning everything.
		await expect(
			services.handle("artifacts.read", { id: put.id, options: { offset: 0, length: 1_048_577 } }, context),
		).rejects.toThrow("Invalid artifact range");
	});
});
