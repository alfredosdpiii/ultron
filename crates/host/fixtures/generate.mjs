// Generates crates/host/fixtures/host.json: the TypeScript session worker's host operations (blocking bash from
// ultron/rlm/host-bash.ts, shell jobs from ultron/rlm/shell-jobs.ts, routed like experimental/session-worker.ts) serving
// the real runtime.py through the TS kernel supervisor (kernel.ts), over a scripted list of cells in a temp dir.
// Results are normalized (job ids, temp paths) so the Rust host replaying the same cells must match exactly.
// Usage: node crates/host/fixtures/generate.mjs
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const src = join(root, "packages/coding-agent/src");
const rlm = join(src, "ultron/rlm");
const load = (path) => import(pathToFileURL(join(src, path)).href);
const { RlmKernel } = await load("ultron/rlm/kernel.ts");
const { ShellJobs } = await load("ultron/rlm/shell-jobs.ts");
const { runHostBash } = await load("ultron/rlm/host-bash.ts");
const { createLocalBashOperations } = await load("core/tools/bash.ts");
const { stripAnsi } = await load("utils/ansi.ts");
const { sanitizeBinaryOutput } = await load("utils/shell.ts");
const { truncateTail } = await load("core/tools/truncate.ts");
const { cells } = JSON.parse(await (await import("node:fs/promises")).readFile(join(here, "cells.json"), "utf8"));

export function normalize(text, cwd, jobs) {
	return text
		.split(jobs).join("<jobs>")
		.split(cwd).join("<cwd>")
		.replace(/job-[0-9a-f]{8}/g, "job-ID")
		.replace(/pi-bash-[0-9a-f]{16}/g, "pi-bash-ID");
}

const cwd = mkdtempSync(join(tmpdir(), "ultron-host-fixture-"));
const jobs = join(cwd, ".jobs");
let saved;
const shell = new ShellJobs({
	cwd,
	dir: jobs,
	operations: () => createLocalBashOperations(),
	store: { read: async () => saved, write: async (doc) => { saved = doc; } },
});
const caller = { lane: "main" };
const api = {};
const handler = async (type, payload, signal) => {
	const context = signal ? { abortSignal: signal } : {};
	if (type === "bash" && payload.yield_after !== undefined)
		return shell.module.handle({ type: "shell.bash", payload, caller, context }, api);
	if (type === "bash") return runHostBash(payload, cwd, createLocalBashOperations(), signal);
	if (type.startsWith("shell.")) return shell.module.handle({ type, payload, caller, context }, api);
	throw new Error(`Ultron RLM host request is not available in this host yet: ${type}`);
};
const kernel = new RlmKernel(
	{ cwd, runtimePath: join(rlm, "runtime.py"), treeMemoryBackend: "off", env: { ULTRON_RLM_SPILL_DIR: "", TMPDIR: cwd } },
	handler,
);
const results = [];
try {
	for (const code of cells) {
		try {
			const result = await kernel.execute(code);
			const { status, stdout, result: value, error } = result;
			results.push({ code, ok: JSON.parse(normalize(JSON.stringify({ status, stdout, result: value ?? null, error: error ? `${error.ename}: ${error.evalue}` : null }), cwd, jobs)) });
		} catch (error) {
			results.push({ code, error: normalize(error.message, cwd, jobs) });
		}
	}
} finally {
	await kernel.shutdown();
	await shell.close();
	rmSync(cwd, { recursive: true, force: true });
}
// Chunk cleaning (stripAnsi, sanitizeBinaryOutput, \r removal) and truncateTail over an input table.
const E = "\u001b";
const cleanInputs = [
	"plain", `${E}[31mred${E}[0m`, `${E}[12345m`, `${E}[1;2;3m`, `${E}[?25h`, `${E}[1:2:3m`, `${E}]0;title\u0007x`,
	`${E}]8;;http://a${E}\\link${E}]8;;${E}\\`, `${E}]unterminated`, `${E}[`, `${E}x`, "\u009b31mc1", `${E}[(B`,
	"a\r\nb\u0000c\u0001\u001f\td", "\ufff9\ufffa\ufffb\ufffc", "\u007f\u0080", `${E}[12;m`, `${E}[99999;1m`, "é😀",
];
const clean = cleanInputs.map((input) => ({ input, output: sanitizeBinaryOutput(stripAnsi(input)).replace(/\r/g, "") }));
const line = "x".repeat(99);
const tailInputs = [
	"", "a\nb\n", Array.from({ length: 2000 }, (_, i) => `${i}`).join("\n"), Array.from({ length: 2001 }, (_, i) => `${i}`).join("\n"),
	`${Array.from({ length: 2001 }, (_, i) => `${i}`).join("\n")}\n`, Array.from({ length: 600 }, () => line).join("\n"),
	"y".repeat(60_000), `head\n${"é".repeat(30_000)}`, `${"z".repeat(51_199)}\n${"w".repeat(10)}`,
];
const tail = tailInputs.map((input) => {
	const result = truncateTail(input);
	return { input, truncated: result.truncated, output: result.content };
});
const decoderInputs = [[0xe2, 0x82, 0xac], [0xe2], [0xe2, 0x82], [0xff, 0x41], [0xe2, 0x41], [0xf0, 0x9f, 0x98], [0xed, 0xa0, 0x80], [0xc0, 0xaf, 0x41]];
const decoder = decoderInputs.flatMap((bytes) => {
	// Every split point of the byte string into two streamed chunks, then end of stream.
	return Array.from({ length: bytes.length + 1 }, (_, cut) => {
		const d = new TextDecoder();
		const text = d.decode(Uint8Array.from(bytes.slice(0, cut)), { stream: true }) + d.decode(Uint8Array.from(bytes.slice(cut)), { stream: true }) + d.decode();
		return { bytes, cut, text };
	});
});
writeFileSync(join(here, "host.json"), `${JSON.stringify({ results, clean, tail, decoder }, null, "\t")}\n`);
