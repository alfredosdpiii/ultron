// Generates crates/kernel/fixtures/snapshot.json from the TypeScript RLM kernel supervisor (kernel.ts) and
// snapshot-auth.ts in this checkout, driving the real runtime.py behind fixtures/tap.py.
// Records: signSnapshot / verifySnapshot over an input table, and scripted kernels exercising snapshot, explicit
// restore, startup restore from a configured snapshotPath, and refused (tampered, unsigned, foreign-key) snapshots:
// every result, the signed snapshot file, and every protocol line in both directions per kernel.
// Usage: node crates/kernel/fixtures/generate-snapshot.mjs
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const rlm = join(root, "packages/coding-agent/src/ultron/rlm");
const { RlmKernel } = await import(pathToFileURL(join(rlm, "kernel.ts")).href);
const { signSnapshot, verifySnapshot } = await import(pathToFileURL(join(rlm, "snapshot-auth.ts")).href);

const key = Buffer.alloc(32, 7);
const otherKey = Buffer.alloc(32, 9);

// signSnapshot / verifySnapshot table.
const signed = (content) => signSnapshot(key, Buffer.from(content, "utf8")).toString("utf8");
const contents = ["", "abc\n", 'header\n{"names":{}}', "é ☃ \u0000 bytes"];
const signCases = contents.map((content) => ({ content, signed: signed(content) }));
const good = signed('{"format":"ultron-rlm-snapshot"}\nbody');
const [goodLine, ...rest] = good.split("\n");
const goodBody = rest.join("\n");
const record = JSON.parse(goodLine);
const verifyInputs = [
	good,
	`${goodLine}\nbodY`,
	goodBody,
	"",
	"no newline at all",
	`not json\n${goodBody}`,
	`${JSON.stringify({ ...record, version: 2 })}\n${goodBody}`,
	`${JSON.stringify({ ...record, alg: "HMAC-SHA512" })}\n${goodBody}`,
	`${JSON.stringify({ ...record, hmac: 7 })}\n${goodBody}`,
	`${JSON.stringify({ ...record, hmac: record.hmac.toUpperCase() })}\n${goodBody}`,
	`${JSON.stringify({ ...record, hmac: record.hmac.slice(2) })}\n${goodBody}`,
	`${JSON.stringify({ ...record, format: "other" })}\n${goodBody}`,
	`[1]\n${goodBody}`,
	signSnapshot(otherKey, Buffer.from(goodBody)).toString("utf8"),
];
const verifyCases = verifyInputs.map((input) => {
	try {
		return { input, ok: verifySnapshot(key, Buffer.from(input, "utf8")).contentSha256 };
	} catch (error) {
		return { input, error: error.message, name: error.name };
	}
});

/** Restorable plain data, with exact types that JSON alone would silently change (A08). */
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

/** Every entry is state the snapshot cannot restore faithfully (A08). */
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

const RESTORED = ["blob", "by_number", "frozen", "mutable_blob", "pair", "plain", "state", "tags"];
const CHECK = `
${PLAIN_DATA.replace(/^(\w+) =/gm, "expected_$1 =").replace('state["counter"] = 3', "")}
def same(left, right):
    if type(left) is not type(right):
        return False
    if isinstance(left, (list, tuple)):
        return len(left) == len(right) and all(same(a, b) for a, b in zip(left, right))
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(same(left[k], right[k]) for k in left)
    return left == right
[name for name in ${JSON.stringify(RESTORED.filter((name) => name !== "state"))} if not same(globals()[name], globals()["expected_" + name])] + ([] if state == {"counter": 3} else ["state"])
`;

/** Scripted kernels. Each step: execute, snapshot, restore (with a path label), or write (replace a file). */
const kernels = [
	{ name: "source", steps: [["execute", PLAIN_DATA], ["execute", UNSUPPORTED], ["snapshot", "main"], ["read", "main"],
		["execute", "balance = 100"], ["snapshot", "main"], ["read", "main"]] },
	{ name: "explicit restore", steps: [["execute", "marker = 'kept'"], ["restore", "missing"], ["copy", "main", "tampered", [['"$dict"', '"$dicT"'], ["-7", "-8"]]],
		["restore", "tampered"], ["unsign", "main", "unsigned"], ["restore", "unsigned"], ["write", "garbage", "\u0080\u0004cos\nsystem\n"], ["restore", "garbage"],
		["foreign", "main", "foreign"], ["restore", "foreign"], ["execute", "(marker, 'plain' in globals())"],
		["restore", "main"], ["execute", CHECK], ["execute", "(marker, 'sock' in globals(), balance)"]] },
	{ name: "startup restore", snapshotPath: "main", steps: [["execute", CHECK], ["execute", "'function' in globals(), balance"]] },
	{ name: "startup restore refused", snapshotPath: "tampered", steps: [["execute", "state"], ["execute", "state"], ["restore", "tampered"],
		["execute", "('plain' in globals(), state)"]] },
	{ name: "startup restore missing", snapshotPath: "missing", steps: [["execute", "'plain' in globals()"]] },
];

function normalizeWire(text, dir) {
	const ids = new Map();
	const wire = { h2k: [], k2h: [] };
	for (const entry of text.split("\n").filter(Boolean).map((line) => JSON.parse(line))) {
		const line = entry.line
			.replace(/[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/g, (id) => {
				if (!ids.has(id)) ids.set(id, `req-${ids.size + 1}`);
				return ids.get(id);
			})
			.replace(/"pid":[0-9]+/, '"pid":0')
			.replaceAll(dir, "<dir>")
			.replaceAll(root, "<root>");
		wire[entry.dir].push(line);
	}
	return wire;
}

async function run() {
	const dir = mkdtempSync(join(tmpdir(), "ultron-snapshot-fixture-"));
	const file = (label) => join(dir, `${label}.snapshot`);
	const normalize = (value) => JSON.parse(JSON.stringify(value).replaceAll(dir, "<dir>"));
	const out = [];
	try {
		for (const spec of kernels) {
			const log = join(dir, `${spec.name.replaceAll(" ", "-")}.log`);
			const kernel = new RlmKernel(
				{
					cwd: dir,
					runtimePath: join(here, "tap.py"),
					treeMemoryBackend: "off",
					limits: { maxMemoryMb: 4096, maxCpuSeconds: 1800 },
					snapshotKey: key,
					...(spec.snapshotPath ? { snapshotPath: file(spec.snapshotPath) } : {}),
					env: {
						ULTRON_TAP_RUNTIME: join(rlm, "runtime.py"),
						ULTRON_TAP_LOG: log,
						ULTRON_RLM_SPILL_DIR: "",
						ULTRON_RLM_OUTPUT_BYTES: "20000",
						// Set and frozenset iteration order (and so the snapshot bytes) depends on string hashing.
						PYTHONHASHSEED: "0",
					},
				},
				() => null,
			);
			const steps = [];
			try {
				for (const step of spec.steps) {
					const [kind, arg] = step;
					if (kind === "read") {
						steps.push({ step, file: readFileSync(file(arg), "utf8") });
						continue;
					}
					if (kind === "copy" || kind === "unsign" || kind === "foreign" || kind === "write") {
						let text = kind === "write" ? step[2] : readFileSync(file(arg), "utf8");
						if (kind === "copy") for (const [from, to] of step[3]) text = text.replace(from, to);
						if (kind === "unsign") text = text.split("\n").slice(1).join("\n");
						if (kind === "foreign") {
							const content = Buffer.from(text.split("\n").slice(1).join("\n"), "utf8");
							text = signSnapshot(otherKey, content).toString("utf8");
						}
						writeFileSync(file(kind === "write" ? arg : step[2]), text);
						steps.push({ step });
						continue;
					}
					try {
						const result = await (kind === "execute"
							? kernel.execute(arg)
							: kind === "snapshot"
								? kernel.snapshot(file(arg))
								: kernel.restore(file(arg)));
						steps.push({ step, ok: normalize(result) });
					} catch (error) {
						steps.push({ step, error: normalize(error.message) });
					}
				}
			} finally {
				await kernel.shutdown();
			}
			out.push({ name: spec.name, snapshotPath: spec.snapshotPath ?? null, steps, wire: normalizeWire(readFileSync(log, "utf8"), dir) });
		}
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
	return out;
}

const fixture = {
	source: "packages/coding-agent/src/ultron/rlm/kernel.ts, snapshot-auth.ts",
	key: key.toString("hex"),
	otherKey: otherKey.toString("hex"),
	sign: signCases,
	verify: verifyCases,
	kernels: await run(),
};
writeFileSync(join(here, "snapshot.json"), `${JSON.stringify(fixture, null, 1)}\n`);
