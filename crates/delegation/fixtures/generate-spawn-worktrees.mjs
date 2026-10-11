// Generates crates/delegation/fixtures/spawn-worktrees.json from NativeRlmHost (native-host.ts) with a shared Git
// workspace and scripted `externalChild` runs: worktree children write files in their own checkout (run.cwd), call
// rlm.finish, and reply; the root collects (verdict checks against workspace snapshots, worktree commits) and merges
// (clean, conflicting with stop/skip/markers, skipped failures, already merged), lists and cleans up worktrees.
// Records every host call's result or error, each child's run inputs (brief, cwd, env), files and branches after the
// merges, completion events and the journal. Temp roots, task ids, worktree ids, keys and Git ids are normalized.
// Usage: node crates/delegation/fixtures/generate-spawn-worktrees.mjs
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const { NativeRlmHost } = await import(join(here, "../../../packages/coding-agent/src/ultron/rlm/native-host.ts"));

const passed = (files) => ({ status: "passed", summary: "did it", evidence: ["cargo test: exit 0, 4 passed"], changed_files: files });
// "<repo>" in a script stands for the workspace's absolute path (a child naming a file in its parent's checkout).
const scenarios = [
	{ name: "worktree children commit, are checked and merge", workspace: { "a.txt": "one\n", "b.txt": "b\n" }, children: [
		{ actions: [["write", "a.txt", "one\ntwo\n"], ["write", "new.txt", "fresh\n"], ["call", "rlm.finish", passed(["a.txt", "<repo>/new.txt"])]], text: "done" },
		{ actions: [["call", "rlm.finish", passed(["ghost.txt"])]], text: "claimed" },
		{ actions: [["write", "shared.txt", "s\n"], ["call", "rlm.finish", passed(["shared.txt"])]], text: "shared" },
		{ actions: [["write", "b.txt", "b2\n"]], text: "no verdict" },
		{ actions: [["write", "c.txt", "c\n"], ["call", "rlm.finish", { status: "failed", summary: "could not finish" }]], text: "failed" },
	], ops: [
		["call", "rlm.spawn", { prompt: "edit a", kwargs: { name: "editor", worktree: true } }], ["settle"],
		["call", "rlm.spawn", { prompt: "claim", kwargs: { name: "claimer", worktree: true } }], ["settle"],
		["call", "rlm.spawn", { prompt: "shared", kwargs: { name: "sharer" } }], ["settle"],
		["call", "rlm.spawn", { prompt: "b", kwargs: { name: "silent", worktree: "auto" } }], ["settle"],
		["call", "rlm.spawn", { prompt: "c", kwargs: { name: "failer", worktree: true } }], ["settle"],
		["call", "rlm.collect", {}],
		["call", "rlm.worktrees.list", {}],
		["call", "rlm.merge", {}],
		["files", ["a.txt", "b.txt", "c.txt", "new.txt", "shared.txt"]], ["branches"], ["status"],
		["call", "rlm.merge", { selectors: ["$0"] }],
		["call", "rlm.merge", { selectors: ["$4", "$2"], include_failed: true }],
		["call", "rlm.merge", { selectors: ["$0", "$0"] }],
		["call", "rlm.merge", { selectors: ["nope"] }],
		["call", "rlm.merge", { on_conflict: "explode" }],
		["call", "rlm.worktrees.list", { all: true }],
		["call", "rlm.worktrees.cleanup", {}],
		["branches"],
	]},
	{ name: "conflicting worktree children stop, skip and write markers", workspace: { "a.txt": "1\nshared\n3\n", "z.txt": "z\n" }, children: [
		{ actions: [["write", "a.txt", "1\nfirst\n3\n"], ["call", "rlm.finish", passed(["a.txt"])]], text: "one" },
		{ actions: [["write", "a.txt", "1\nsecond\n3\n"], ["call", "rlm.finish", passed(["a.txt"])]], text: "two" },
		{ actions: [["write", "z.txt", "z2\n"], ["call", "rlm.finish", passed(["z.txt"])]], text: "three" },
	], ops: [
		["call", "rlm.spawn", { prompt: "one", kwargs: { name: "one", worktree: true } }], ["settle"],
		["call", "rlm.spawn", { prompt: "two", kwargs: { name: "two", worktree: true } }], ["settle"],
		["call", "rlm.spawn", { prompt: "three", kwargs: { name: "three", worktree: true } }], ["settle"],
		["call", "rlm.merge", {}],
		["files", ["a.txt", "z.txt"]],
		["call", "rlm.merge", { selectors: ["$1", "$2"], on_conflict: "skip" }],
		["files", ["a.txt", "z.txt"]],
		["call", "rlm.merge", { selectors: ["$1"], on_conflict: "markers" }],
		["files", ["a.txt"]], ["branches"],
		["call", "rlm.worktrees.list", {}],
	]},
	{ name: "a worktree child's own children work in its worktree", workspace: { "a.txt": "a\n" }, children: [
		{ actions: [["call", "rlm.spawn", { prompt: "inner", kwargs: { name: "inner" } }], ["call", "rlm.collect", {}], ["call", "rlm.finish", passed(["inner.txt"])]], text: "outer done" },
		{ actions: [["write", "inner.txt", "i\n"], ["call", "rlm.finish", passed(["inner.txt"])]], text: "inner done" },
	], ops: [
		["call", "rlm.spawn", { prompt: "outer", kwargs: { name: "outer", worktree: true, depth: 1 } }], ["settle"],
		["call", "rlm.collect", {}],
		["call", "rlm.merge", {}],
		["files", ["inner.txt"]],
	]},
	{ name: "worktree=auto falls back outside Git; worktree=True does not", workspace: null, plain: true, children: [
		{ actions: [["write", "p.txt", "p\n"], ["call", "rlm.finish", passed(["p.txt"])]], text: "plain" },
	], ops: [
		["call", "rlm.spawn", { prompt: "auto", kwargs: { name: "auto", worktree: "auto" } }], ["settle"],
		["call", "rlm.spawn", { prompt: "strict", kwargs: { name: "strict", worktree: true } }],
		["call", "rlm.spawn", { prompt: "x", kwargs: { name: "x", worktree_setup: { command: "true" } } }],
		["call", "rlm.collect", {}],
		["call", "rlm.merge", {}],
	]},
	{ name: "no workspace at all", workspace: undefined, children: [], ops: [
		["call", "rlm.spawn", { prompt: "strict", kwargs: { name: "strict", worktree: true } }],
		["call", "rlm.merge", {}],
		["call", "rlm.worktrees.cleanup", {}],
	]},
];

const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const settle = async () => {
	for (let n = 0; n < 200; n++) await new Promise((r) => setTimeout(r, 2));
};
const QUIET = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"];
const results = [];
for (const s of scenarios) {
	const root = realpathSync(mkdtempSync(join(tmpdir(), "ultron-spawn-wt-")));
	let workspace;
	if (s.workspace !== undefined) {
		workspace = join(root, "repo");
		mkdirSync(workspace, { recursive: true });
		if (s.workspace !== null) {
			execFileSync("git", ["init", "-q", "-b", "main"], { cwd: workspace });
			execFileSync("git", ["config", "user.name", "Fixture"], { cwd: workspace });
			execFileSync("git", ["config", "user.email", "fixture@example.com"], { cwd: workspace });
			for (const [path, text] of Object.entries(s.workspace)) writeFileSync(join(workspace, path), text);
			execFileSync("git", [...QUIET, "add", "-A"], { cwd: workspace });
			execFileSync("git", [...QUIET, "commit", "-q", "-m", `initial: ${s.name}`], { cwd: workspace });
		}
	}
	let value;
	const store = { read: async () => clone(value) ?? undefined, write: async (d) => void (value = clone(d)) };
	const events = [];
	const runs = [];
	let host;
	let spawned = 0;
	const worktreeIds = [];
	host = new NativeRlmHost({ lane: async () => { throw new Error("no harness lanes in this fixture"); } }, {}, {
		store,
		now: () => 1000,
		...(workspace === undefined ? {} : { workspace }),
		sessionId: "fixture-session",
		externalChild: async (run) => {
			const index = spawned++;
			const script = s.children[index];
			const record = { index, taskId: run.taskId, prompt: run.prompt, cwd: run.cwd ?? null, env: run.env ?? null, calls: [] };
			runs.push(record);
			const cwd = run.cwd ?? workspace;
			for (const action of script.actions) {
				if (action[0] === "write") {
					mkdirSync(dirname(join(cwd, action[1])), { recursive: true });
					writeFileSync(join(cwd, action[1]), action[2]);
					continue;
				}
				const payload = JSON.parse(JSON.stringify(action[2]).split("<repo>").join(workspace ?? ""));
				try { record.calls.push({ type: action[1], result: clone(await host.handle(action[1], payload, {}, { lane: run.laneName })) }); }
				catch (e) { record.calls.push({ type: action[1], error: e.message }); }
			}
			return { text: script.text, turns: 1, toolCalls: 0 };
		},
		onDetachedEnd: (end) => events.push({ task: clone(end.task), kind: end.kind, ownerLane: end.ownerLane, awaited: end.awaited, fetch: end.fetch }),
	});
	const steps = [];
	const ids = [];
	const resolveArgs = (payload) => JSON.parse(JSON.stringify(payload).replace(/"\$(\d+)"/g, (_, n) => JSON.stringify(ids[Number(n)])));
	for (const op of s.ops) {
		if (op[0] === "call") {
			try {
				const r = clone(await host.handle(op[1], resolveArgs(op[2]), {}));
				if (op[1] === "rlm.spawn") {
					ids.push(r.rlm_child_id);
					if (r.worktree) worktreeIds.push(r.worktree.path.split("/").pop());
				}
				steps.push({ op, result: r });
			} catch (e) { steps.push({ op, error: e.message }); }
		} else if (op[0] === "files") {
			steps.push({ op, result: op[1].map((path) => { try { return readFileSync(join(workspace, path), "utf8"); } catch { return null; } }) });
		} else if (op[0] === "branches") {
			steps.push({ op, result: execFileSync("git", ["for-each-ref", "--format=%(refname:short)", "refs/heads"], { cwd: workspace, encoding: "utf8" }).split("\n").filter(Boolean) });
		} else if (op[0] === "status") {
			steps.push({ op, result: execFileSync("git", ["status", "--porcelain=v1", "--untracked-files=all"], { cwd: workspace, encoding: "utf8" }) });
		} else {
			await settle();
			steps.push({ op });
		}
	}
	await settle();
	await host.close();
	results.push({ name: s.name, workspace: s.workspace === undefined ? "none" : s.workspace === null ? "plain" : "git", files: s.workspace ?? null, children: s.children, steps, runs, events, journal: clone(value), root, worktreeIds });
	rmSync(root, { recursive: true, force: true });
}

let text = JSON.stringify(results, null, "\t");
for (const r of results) {
	text = text.split(r.root).join("<root>");
	r.worktreeIds.forEach((id, n) => { text = text.split(id).join(`wt#${n}`); });
}
const seen = new Map();
text = text.replace(/ultron-task-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, (id) => {
	if (!seen.has(id)) seen.set(id, `ultron-task-#${seen.size}`);
	return seen.get(id);
});
const keys = new Map();
text = text.replace(/"key": "([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"/g, (_, key) => {
	if (!keys.has(key)) keys.set(key, `key-#${keys.size}`);
	return `"key": "${keys.get(key)}"`;
});
const hashes = new Map();
text = text.replace(/\b[0-9a-f]{40}\b/g, (h) => {
	if (!hashes.has(h)) hashes.set(h, `#${hashes.size}`);
	return hashes.get(h);
});
const parsed = JSON.parse(text);
for (const r of parsed) {
	delete r.root;
	delete r.worktreeIds;
}
writeFileSync(join(here, "spawn-worktrees.json"), `${JSON.stringify(parsed, null, "\t")}\n`);
