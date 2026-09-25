import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";

// A08: unsupported Python state reports non-restorable, never silently correct.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const directories: string[] = [];
const kernels: RlmKernel[] = [];

afterEach(async () => {
	await Promise.all(kernels.splice(0).map((kernel) => kernel.shutdown()));
	for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function snapshotFile(): string {
	const directory = mkdtempSync(join(tmpdir(), "ultron-a08-"));
	directories.push(directory);
	return join(directory, "kernel.snapshot");
}

function kernel(snapshotPath?: string): RlmKernel {
	const instance = new RlmKernel({ cwd: process.cwd(), runtimePath, snapshotPath }, () => {
		throw new Error("no host services in this fixture");
	});
	kernels.push(instance);
	return instance;
}

/** Restorable plain data, with exact types that JSON alone would silently change. */
const PLAIN_DATA = `
plain = {
    "none": None, "flag": True, "count": -7, "ratio": 0.25, "text": "héllo \\u2603",
    "items": [1, [2, (3, 4)], {"k": b"raw"}],
}
pair = (1, "two", (3.0, None))
tags = {"a", "b"}
frozen = frozenset({1, 2})
blob = b"\\x00\\xffbytes"
mutable_blob = bytearray(b"abc")
by_number = {1: "one", (2, 3): "tuple key", None: "none key"}
state["counter"] = 3
`;

/** Every entry is state the snapshot cannot restore faithfully. */
const UNSUPPORTED = `
import math, socket, tempfile, enum
def function(x):
    return x + 1
lambda_value = lambda x: x * 2
generator = (n for n in range(3))
open_file = tempfile.TemporaryFile()
sock = socket.socket()
class Point:
    def __init__(self, x):
        self.x = x
instance = Point(1)
module = math
class Color(str, enum.Enum):
    RED = "red"
str_subclass = Color.RED
cyclic = [1]
cyclic.append(cyclic)
shared_a = [1, 2]
shared_b = shared_a
huge = 10 ** 5000
`;

const UNSUPPORTED_NAMES = [
	"Color",
	"Point",
	"cyclic",
	"enum",
	"function",
	"generator",
	"huge",
	"instance",
	"lambda_value",
	"math",
	"module",
	"open_file",
	"shared_a",
	"shared_b",
	"sock",
	"socket",
	"str_subclass",
	"tempfile",
];

const RESTORED_NAMES = ["blob", "by_number", "frozen", "mutable_blob", "pair", "plain", "state", "tags"];

/** Compare restored values with freshly built expected ones, including every nested type. */
const CHECK_EQUAL = `
${PLAIN_DATA.replace(/^(\w+) =/gm, "expected_$1 =").replace('state["counter"] = 3', "")}
def same(left, right):
    if type(left) is not type(right):
        return False
    if isinstance(left, (list, tuple)):
        return len(left) == len(right) and all(same(a, b) for a, b in zip(left, right))
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(same(left[k], right[k]) for k in left)
    return left == right
[name for name in ${JSON.stringify(RESTORED_NAMES.filter((name) => name !== "state"))} if not same(globals()[name], globals()["expected_" + name])] + ([] if state == {"counter": 3} else ["state"])
`;

async function saveMatrix(path: string) {
	const source = kernel(path);
	expect(await source.execute(PLAIN_DATA)).toMatchObject({ status: "ok" });
	expect(await source.execute(UNSUPPORTED)).toMatchObject({ status: "ok" });
	return source.snapshot(path);
}

describe("A08 snapshot restorability and tamper rejection", () => {
	test("snapshot type matrix saves plain data and reports every unsupported name with a reason", async () => {
		const path = snapshotFile();
		const saved = await saveMatrix(path);
		expect(saved.status).toBe("ok");
		expect(saved.snapshot?.saved).toEqual(RESTORED_NAMES);
		expect(saved.snapshot?.skipped).toEqual(UNSUPPORTED_NAMES);
		const reasons = saved.snapshot!.reasons;
		expect(reasons.function).toMatch(/unsupported type builtins\.function/);
		expect(reasons.lambda_value).toMatch(/unsupported type builtins\.function/);
		expect(reasons.generator).toMatch(/unsupported type builtins\.generator/);
		expect(reasons.open_file).toMatch(/unsupported type/);
		expect(reasons.sock).toMatch(/unsupported type socket\.socket/);
		expect(reasons.instance).toMatch(/unsupported type __main__\.Point/);
		expect(reasons.module).toMatch(/unsupported type builtins\.module/);
		expect(reasons.str_subclass).toMatch(/unsupported type __main__\.Color/);
		expect(reasons.cyclic).toMatch(/cyclic/);
		expect(reasons.shared_b).toMatch(/aliases mutable data of shared_a/);
		expect(reasons.shared_a).toMatch(/aliases mutable data of shared_b/);
		expect(reasons.huge).toMatch(/not encodable/);
		// Nothing was written for skipped names, so nothing can come back silently wrong.
		const body = JSON.parse(readFileSync(path, "utf8").split("\n")[1]) as { names: Record<string, unknown> };
		expect(Object.keys(body.names).sort()).toEqual(RESTORED_NAMES);
	});

	test("restore brings back equal values of the same types and reports the non-restorable names as absent", async () => {
		const path = snapshotFile();
		expect((await saveMatrix(path)).status).toBe("ok");

		const target = kernel();
		const restored = await target.restore(path);
		expect(restored.status).toBe("ok");
		expect(restored.restore).toEqual({
			restored: RESTORED_NAMES,
			missing: false,
			skipped: UNSUPPORTED_NAMES,
			reasons: expect.objectContaining({ generator: expect.stringContaining("generator") }),
		});
		expect(await target.execute(CHECK_EQUAL)).toMatchObject({ status: "ok", result: "[]" });
		// Non-restorable names are absent, not replaced by placeholders or stale copies.
		expect(
			await target.execute(`[name for name in ${JSON.stringify(UNSUPPORTED_NAMES)} if name in globals()]`),
		).toMatchObject({ result: "[]" });
		// Restored values are live data: later cells compute with them.
		expect(await target.execute("mutable_blob.extend(b'd'); (bytes(mutable_blob), pair[2][0] + 1)")).toMatchObject({
			result: "(b'abcd', 4.0)",
		});
	});

	test("startup restore from a configured snapshot reports the same non-restorable names", async () => {
		const path = snapshotFile();
		expect((await saveMatrix(path)).status).toBe("ok");
		const restarted = kernel(path);
		expect(await restarted.execute(CHECK_EQUAL)).toMatchObject({ status: "ok", result: "[]" });
		expect(await restarted.execute("'sock' in globals() or 'function' in globals()")).toMatchObject({
			result: "False",
		});
	});

	test("a tampered snapshot body is rejected and leaves the namespace untouched", async () => {
		const path = snapshotFile();
		expect((await saveMatrix(path)).status).toBe("ok");
		const original = readFileSync(path, "utf8");
		writeFileSync(path, original.replace('"$dict"', '"$dicT"').replace("-7", "-8"));

		const target = kernel();
		expect(await target.execute("marker = 'kept'")).toMatchObject({ status: "ok" });
		const restored = await target.restore(path);
		expect(restored).toMatchObject({
			status: "error",
			error: { evalue: expect.stringContaining("integrity check failed: digest mismatch") },
		});
		expect(restored.restore).toBeUndefined();
		expect(await target.execute("(marker, 'plain' in globals())")).toMatchObject({ result: "('kept', False)" });
	});

	test("snapshots without a valid header or with a forged digest over malformed data are rejected", async () => {
		const path = snapshotFile();
		const target = kernel();
		expect(await target.execute("marker = 1")).toMatchObject({ status: "ok" });

		// Legacy/unsigned JSON and arbitrary pickle-like bytes are not loaded.
		for (const content of [
			JSON.stringify({ version: 1, names: { injected: 1 }, skipped: [] }),
			"\x80\x04\x95cos\nsystem\n",
			"",
		]) {
			writeFileSync(path, content);
			expect(await target.restore(path)).toMatchObject({
				status: "error",
				error: { evalue: expect.stringContaining("integrity check failed") },
			});
		}

		// A truncated snapshot fails the digest length check.
		expect((await target.snapshot(path)).status).toBe("ok");
		writeFileSync(path, readFileSync(path, "utf8").slice(0, -5));
		expect(await target.restore(path)).toMatchObject({ status: "error", error: { evalue: /digest mismatch/ } });

		// A writer that recomputes the digest still cannot smuggle non-data values past validation.
		const body = JSON.stringify({ names: { injected: { $object: ["os", "system"] } }, skipped: [], reasons: {} });
		const header = JSON.stringify({
			format: "ultron-rlm-snapshot",
			version: 2,
			sha256: createHash("sha256").update(body).digest("hex"),
			bytes: Buffer.byteLength(body),
		});
		writeFileSync(path, `${header}\n${body}`);
		expect(await target.restore(path)).toMatchObject({ status: "error", error: { evalue: /unknown tag/ } });
		expect(await target.execute("('injected' in globals(), marker)")).toMatchObject({ result: "(False, 1)" });
	});

	test("a kernel configured with a tampered snapshot fails startup instead of starting with wrong state", async () => {
		const path = snapshotFile();
		expect((await saveMatrix(path)).status).toBe("ok");
		writeFileSync(path, readFileSync(path, "utf8").replace('"counter"', '"countex"'));
		const restarted = kernel(path);
		await expect(restarted.execute("state")).rejects.toThrow(
			/startup restore failed: snapshot integrity check failed/,
		);
		// A later explicit restore attempt reports the same failure; nothing was loaded.
		expect(await restarted.restore(path)).toMatchObject({ status: "error", error: { evalue: /integrity/ } });
		expect(await restarted.execute("('plain' in globals(), state)")).toMatchObject({ result: "(False, {})" });
	});
});
