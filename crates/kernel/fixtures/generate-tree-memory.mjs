// Generates crates/kernel/fixtures/tree-memory.json from the TypeScript tree-memory module in this checkout
// (packages/coding-agent/src/ultron/rlm/tree-memory.ts): kernelTreeMemoryLimit over an input table, treeMemoryBackend
// for the choices that never probe the system (a zero cap, "off", "watchdog"), cgroupScopeCommand, watchdogPollMs,
// and cgroupOomKills / cgroupPids over scratch cgroup directories. The cgroup probe itself (systemd-run) is not run.
// Usage: node crates/kernel/fixtures/generate-tree-memory.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const tree = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/rlm/tree-memory.ts")).href);

const outcome = (run) => {
	try {
		return { ok: run() };
	} catch (error) {
		return { error: error.message };
	}
};

const limits = [
	[{}, 4096, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: "1000" }, 4096, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: "lots" }, 512, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: "1000" }, 4096, 64],
	[{}, 0, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: " 77 " }, 10, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: "-5" }, 10, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: "1e3" }, 10, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: "" }, 10, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: "0" }, 10, undefined],
	[{ ULTRON_RLM_MAX_TREE_MEMORY_MB: "99999999999999999999" }, 10, undefined],
	[{}, 10, 9007199254740992],
	[{}, 10, 0],
].map(([env, perProcess, override]) => ({
	env,
	perProcess,
	override: override ?? null,
	result: outcome(() => tree.kernelTreeMemoryLimit(env, perProcess, override)),
}));

const backends = [
	[0, "cgroup"],
	[0, "auto"],
	[100, "off"],
	[100, " OFF "],
	[100, "watchdog"],
	[100, "\tWatchDog\n"],
].map(([cap, requested]) => ({ cap, requested, result: tree.treeMemoryBackend(cap, requested) }));

const polls = [];
for (const cap of [100, 1000, 3])
	for (const bytes of [0, 1, 39, 40, 41, 69, 70, 71, 100, 101, 300, 700, 1000, 2, 3])
		polls.push({ bytes, cap, result: tree.watchdogPollMs(bytes, cap) });

const scratch = mkdtempSync(join(tmpdir(), "ultron-tree-memory-fixture-"));
let cgroups;
try {
	const cases = [
		["missing", null],
		["empty", ""],
		["none", "low 0\nhigh 0\nmax 3\noom 2\n"],
		["kills", "low 0\nhigh 0\nmax 3\noom 2\noom_kill 7\noom_group_kill 0\n"],
		["malformed", "oom_kill x\n"],
		["indented", " oom_kill 4\n"],
	];
	cgroups = cases.map(([name, events]) => {
		const dir = join(scratch, name);
		if (events !== null) {
			mkdirSync(dir);
			writeFileSync(join(dir, "memory.events"), events);
		}
		return { name, events, kills: tree.cgroupOomKills(dir) ?? null };
	});
	const procs = join(scratch, "procs");
	mkdirSync(join(procs, "child", "grandchild"), { recursive: true });
	writeFileSync(join(procs, "cgroup.procs"), "12\n34\nx\n\n56");
	writeFileSync(join(procs, "child", "cgroup.procs"), "78\n");
	writeFileSync(join(procs, "child", "grandchild", "cgroup.procs"), "90\n");
	cgroups.push({ name: "pids", pids: tree.cgroupPids(procs).sort((a, b) => a - b), gone: tree.cgroupPids(join(scratch, "gone")) });
} finally {
	rmSync(scratch, { recursive: true, force: true });
}

assertOffline(process.env.ULTRON_GUARD_LOG);
writeFileSync(
	join(here, "tree-memory.json"),
	`${JSON.stringify({ limits, backends, scopeCommand: [64, 320].map((cap) => ({ cap, command: tree.cgroupScopeCommand(cap) })), polls, cgroups }, null, "\t")}\n`,
);
