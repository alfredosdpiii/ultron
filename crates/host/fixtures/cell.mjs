// Generates crates/host/fixtures/cell.json: the tool-result text TypeScript makes of a kernel result
// (experimental/session-worker.ts UltronRlmKernel.executeCell, with its kernel replaced by a scripted result) and
// truncateToolOutput (ultron/rlm/output-truncation.ts) on its own. No spill directory, so the output is deterministic.
// Usage: node crates/host/fixtures/cell.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(resolve(here, "../../.."), "packages/coding-agent/src");
const load = (path) => import(pathToFileURL(join(src, path)).href);
const { UltronRlmKernel } = await load("experimental/session-worker.ts");
const { truncateToolOutput } = await load("ultron/rlm/output-truncation.ts");

const ok = (stdout, stderr, result) => ({ status: "ok", stdout, stderr, ...(result === undefined ? {} : { result }) });
const fail = (stdout, stderr, error) => ({ status: "error", stdout, stderr, ...(error === undefined ? {} : { error }) });
const zde = {
	ename: "ZeroDivisionError",
	evalue: "division by zero",
	traceback: ["Traceback (most recent call last):", '  File "<rlm-cell>", line 1, in <module>', "    1/0", "ZeroDivisionError: division by zero"],
};
const results = [
	ok("x is 42\n", "", "43"),
	ok("", "", undefined),
	ok("", "", ""),
	ok("a\n\n", "warn\n", "None"),
	ok("only out", "", undefined),
	ok("", "err only\n", "'v'"),
	ok("key: AKIAIOSFODNN7EXAMPLE1\n", "", undefined),
	ok("ghp_" + "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8\n", "", undefined),
	ok("x".repeat(25_000), "", undefined),
	ok("é".repeat(12_000) + "\n", "", "1"),
	fail("", "", zde),
	fail("printed first\n", "", zde),
	fail("", "", { ename: "ValueError", evalue: "bad", traceback: ["Traceback (most recent call last):", "  frame"] }),
	fail("", "", { ename: "KeyboardInterrupt", evalue: "", traceback: [] }),
	fail("out\n", "err\n", undefined),
	fail("", "", { ename: "RuntimeError", evalue: "token sk-ant-api03-" + "Ab1".repeat(30) + "AA", traceback: [] }),
];

const kernel = new UltronRlmKernel("/", async () => ({}), undefined, undefined, {});
const cells = [];
for (const result of results) {
	kernel.kernel = { execute: async () => result };
	try {
		const output = await kernel.executeCell("pass", { abortSignal: undefined });
		cells.push({ result, ok: { text: output.text, masked: output.masked } });
	} catch (error) {
		cells.push({ result, error: { message: error.message, ename: error.ename, masked: error.masked } });
	}
}

const texts = [
	["short", 100],
	["a".repeat(2000), 1024],
	["line\n".repeat(500), 1024],
	["é".repeat(700), 1025],
	["é".repeat(700), 1024],
	["€".repeat(500), 1001],
	["😀".repeat(400), 1023],
	["x".repeat(1024), 1024],
	["x".repeat(1025), 1024],
];
const truncations = texts.map(([text, maxBytes]) => ({ text, maxBytes, out: truncateToolOutput(text, maxBytes) }));

writeFileSync(join(here, "cell.json"), `${JSON.stringify({ cells, truncations }, null, 1)}\n`);
