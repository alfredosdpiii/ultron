// Generates crates/host/fixtures/journal.json: the TypeScript shell job journal (ultron/rlm/shell-jobs.ts) restored
// after a worker restart (running jobs become interrupted, tails read back from spill files), its persisted
// document and the shell.list / shell.get / shell.result / shell.read replies; jobSummary over records; and
// utils/shell.ts getShellEnv PATH handling over environments. Deterministic (fixed clock, normalized temp dir).
// Usage: node crates/host/fixtures/journal.mjs
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(resolve(here, "../../.."), "packages/coding-agent/src");
const load = (path) => import(pathToFileURL(join(src, path)).href);
const { ShellJobs, jobSummary } = await load("ultron/rlm/shell-jobs.ts");
const { getShellEnv } = await load("utils/shell.ts");

const NOW = 1_700_000_100_000;
const base = (id, extra) => ({
	id, lane: "main", rootId: null, command: `cmd ${id}`, cwd: "/w", timeout: null, status: "completed", exitCode: 0,
	startedAt: NOW - 12_345, endedAt: NOW - 1_000, outputPath: `<dir>/${id}.log`, outputBytes: 0, retainedBytes: 0,
	tail: "", ...extra,
});
const big = `${"h".repeat(9000)}\n${"é".repeat(3000)}\nlast line\n`;
const scenarios = [
	{ name: "restart", saved: { version: 1, jobs: [
		base("job-00000001", { status: "running", exitCode: null, endedAt: null }),
		base("job-00000002", { outputBytes: 12, retainedBytes: 12 }),
		base("job-00000003", { status: "failed", exitCode: null, error: "spawn bash ENOENT" }),
		base("job-00000004", { status: "running", exitCode: null, endedAt: NOW - 50, lane: "child" }),
		base("job-00000005", { status: "running", exitCode: null, endedAt: null, outputBytes: Buffer.byteLength(big), retainedBytes: Buffer.byteLength(big) }),
		base("job-00000006", { status: "timed_out", exitCode: null, timeout: 2, error: "timed out after 2s; process tree killed", outputBytes: 9_000_000, retainedBytes: 4_194_304, tail: "kept tail\n" }),
	] }, files: { "job-00000001": "one\ntwo\n", "job-00000002": "hello\nworld\n", "job-00000005": big, "job-00000006": "head\n" } },
	{ name: "no version field", saved: { jobs: [base("job-0000000a", { status: "cancelled", exitCode: null })] }, files: {} },
	{ name: "nothing saved", saved: undefined, files: {} },
	{ name: "not a document", saved: [1, 2], files: {} },
	{ name: "unsupported version", saved: { version: 2, jobs: [] }, files: {} },
];
const out = [];
for (const scenario of scenarios) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-journal-fixture-"));
	const norm = (value) => JSON.parse(JSON.stringify(value ?? null).split(dir).join("<dir>"));
	const saved = scenario.saved === undefined ? undefined : JSON.parse(JSON.stringify(scenario.saved).split("<dir>").join(dir));
	for (const [id, text] of Object.entries(scenario.files)) writeFileSync(join(dir, `${id}.log`), text);
	let written;
	const jobs = new ShellJobs({ cwd: dir, dir, operations: () => { throw new Error("no runs"); }, now: () => NOW,
		store: { read: async () => saved, write: async (doc) => { written = doc; } } });
	const call = async (type, payload, lane = "main") => {
		try { return { ok: norm(await jobs.module.handle({ type, payload, caller: { lane }, context: {} }, {})) }; }
		catch (error) { return { error: norm(error.message) }; }
	};
	const replies = {
		list: await call("shell.list", {}),
		child_list: await call("shell.list", {}, "child"),
		get: await call("shell.get", { id: "job-00000001" }),
		get_big: await call("shell.get", { id: "job-00000005" }),
		result: await call("shell.result", { id: "job-00000002", wait: 1 }),
		read: await call("shell.read", { id: "job-00000002", cursor: 6 }),
		child_get_main: await call("shell.get", { id: "job-00000002" }, "child"),
	};
	await jobs.settled();
	const restored = jobs.list();
	out.push({ name: scenario.name, saved: scenario.saved ?? null, files: scenario.files, replies, persisted: norm(written),
		summaries: restored.map((job) => [job.id, jobSummary(job), jobSummary(job, 20)]) });
	await jobs.close();
	rmSync(dir, { recursive: true, force: true });
}
// getShellEnv over environments: the PATH key is found case-insensitively; the bin dir is prepended once.
const original = { ...process.env };
const envs = [
	{ PATH: "/usr/bin:/bin", HOME: "/h" },
	{ Path: "/usr/bin", OTHER: "x" },
	{ PATH: "/a/agent/bin:/usr/bin" },
	{ PATH: "/usr/bin:/a/agent/bin" },
	{ PATH: "" },
	{ HOME: "/h" },
	{ PATH: "::/usr/bin:", path: "/lower" },
	{ PATH: "/a/agent/bin/" },
];
const shellEnv = [];
for (const env of envs) {
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, env, { ULTRON_CODING_AGENT_DIR: "/a/agent" });
	shellEnv.push({ env: { ...process.env }, result: Object.entries(getShellEnv()) });
}
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, original);
writeFileSync(join(here, "journal.json"), `${JSON.stringify({ now: NOW, scenarios: out, shell_env: shellEnv }, null, "\t")}\n`);
