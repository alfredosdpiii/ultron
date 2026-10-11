// Generates crates/delegation/fixtures/verdict.json from packages/coding-agent/src/ultron/rlm/verdict.ts:
// validateVerdict, isConcreteEvidence, workspacePath and verdictTag over a table of inputs.
// Usage: node crates/delegation/fixtures/generate-verdict.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const v = await import(join(here, "../../../packages/coding-agent/src/ultron/rlm/verdict.ts"));

const long = (n, c = "x") => c.repeat(n);
const payloads = [
	{ status: "passed", summary: "ok", evidence: ["npm test: exit 0"] },
	{ status: "passed", summary: "ok" },
	{ status: "passed", summary: "ok", evidence: [] },
	{ status: "passed", summary: "ok", evidence: null, outputs: null, changed_files: null },
	{ status: "passed", summary: "ok", evidence: ["it works\nreally well, trust me, it is all fine and good and done and the end of it all, honestly completely fine"] },
	{ status: "failed", summary: "  spaced  ", evidence: ["  a  ", "b"], outputs: { n: 1 }, changed_files: ["a.txt", "./a.txt", "/ws/b/c.ts", "/elsewhere/x", "d/", "a.txt"] },
	{ status: "blocked", summary: long(2001) },
	{ status: "blocked", summary: "s", outputs: [] },
	{ status: "blocked", summary: "s", outputs: "text" },
	{ status: "blocked", summary: "s", outputs: { big: long(33000) } },
	{ status: "blocked", summary: "s", evidence: "one" },
	{ status: "blocked", summary: "s", evidence: ["ok", ""] },
	{ status: "blocked", summary: "s", evidence: ["ok", 3] },
	{ status: "blocked", summary: "s", evidence: [long(1001)] },
	{ status: "blocked", summary: "s", evidence: Array.from({ length: 41 }, (_, n) => `e${n}`) },
	{ status: "blocked", summary: "s", changed_files: "a" },
	{ status: "blocked", summary: "s", changed_files: ["a", " "] },
	{ status: "blocked", summary: "s", changed_files: Array.from({ length: 501 }, (_, n) => `f${n}`) },
	{ status: 3, summary: 4 },
	{},
	{ status: "PASSED", summary: "x", whatever: 1, more: 2 },
	{ status: "passed", summary: "x", evidence: [long(300, "y")] },
	{ status: "done\n\tnow   later", summary: "x" },
];
const evidence = [
	"npm test: exit 0", "exited with code 1", "exit status: 2", "EXIT=0", "42 passed", "3 failing", "1 file", "12 matches",
	"see src/app.ts", "path ./a/b", "../up/file", "@scope/pkg/x", "README.md", "file.tsx", "a.b", "line :40", "x:40-52",
	"foo:3:7", "all good", "it works", "exit code", "version 1.2", "tests pass", "", "a/b", "(src/x)", "`lib/y`",
];
const paths = [
	["a.txt", "/ws"], ["./a.txt", "/ws"], ["a//b/", "/ws"], ["/ws/x/y", "/ws"], ["/ws", "/ws"], ["/other/z", "/ws"],
	["/ws/../ws/q", "/ws"], ["../out", "/ws"], ["..", "/ws"], [".", "/ws"], ["  spaced.txt  ", "/ws"], ["/abs", null],
	["a/./b/../c", null], ["a/b/..", null], ["", "/ws"], ["/wsx/a", "/ws"],
];
const tags = [
	{},
	{ unverified: true },
	{ unverified: true, check: { outcome: "invalid", unobserved: [], unreported: [] } },
	{ unverified: true, worktree: { branch: "ultron/s/a", commit: "abc", changed_files: ["a", "b"] } },
	{ verdict: { status: "passed" } },
	{ verdict: { status: "failed" }, check: { outcome: "verified", unobserved: [], unreported: [] } },
	{ verdict: { status: "passed" }, check: { outcome: "contradicted", unobserved: ["a", "b", "c", "d"], unreported: ["x", "y"], outside: ["o"] } },
	{ verdict: { status: "blocked" }, check: { outcome: "unchecked", unobserved: [], unreported: [] }, worktree: { branch: "b", commit: "c", changed_files: [] } },
	{ verdict: { status: "passed" }, worktree: { branch: "b", commit: null, changed_files: ["x"] } },
];
const out = {
	validate: payloads.map((payload) => ({ payload, workspace: "/ws", result: v.validateVerdict(payload, "/ws") })),
	concrete: evidence.map((text) => ({ text, concrete: v.isConcreteEvidence(text) })),
	paths: paths.map(([path, ws]) => ({ path, workspace: ws, result: v.workspacePath(path, ws ?? undefined) ?? null })),
	tags: tags.map((result) => ({ result, tag: v.verdictTag(result) ?? null })),
};
writeFileSync(join(here, "verdict.json"), `${JSON.stringify(out, null, "\t")}\n`);
