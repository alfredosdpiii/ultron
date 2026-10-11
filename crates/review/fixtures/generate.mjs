// Generates crates/review/fixtures/review.json from the Python /review implementation in this checkout
// (packages/coding-agent/src/ultron/rlm/review_prompts.py and review_api.py), by running fixtures/record.py.
// It also records the autoreview fates ledger (lessons.ts) to ledger.json.
// The Rust tests compare every recorded output exactly.
// Usage: node crates/review/fixtures/generate.mjs
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const rlm = join(root, "packages/coding-agent/src/ultron/rlm");
const out = execFileSync(python, ["-I", join(here, "record.py"), rlm], { encoding: "utf8", maxBuffer: 64 << 20 });
writeFileSync(join(here, "review.json"), out);

// The fates ledger (packages/coding-agent/src/ultron/autoreview/lessons.ts): pure functions over recorded inputs.
process.env.TZ = "UTC";
const lessons = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/lessons.ts")).href);
const finding = (id, status, extra = {}) => ({ id, file: `f${id}.ts`, line: Number(id.slice(1)) || 1, claim: `claim ${id}`,
	severity: "high", status, ...extra });
const fateCases = [
	{ pull: "github.com/o/r#1", now: "2026-10-01T00:00:00.000Z", findings: [
		finding("a1", "open", { commentId: 11, category: "correctness", kind: "fast" }),
		finding("a2", "open", { commentId: 12 }), finding("a3", "fixed", { commentId: 13, kind: "deep:claims" }),
		finding("a4", "open", { commentId: 14 }), finding("a5", "open", { commentId: 15 }),
		finding("a6", "not_applicable", { commentId: 16 }), finding("a7", "still_present"), finding("a8", "open", { commentId: 99 }),
		finding("a9", "fixed", { commentId: 19 })],
	  statuses: [["a5", "fixed"], ["a7", "open"]],
	  threads: [[11, { id: "t11", resolved: false, thumbsDown: 1 }], [12, { id: "t12", resolved: true, thumbsUp: 2 }],
		[13, { id: "t13", resolved: false, thumbsUp: 1, thumbsDown: 1 }], [14, { id: "t14", resolved: true }],
		[15, { id: "t15", resolved: true }], [16, { id: "t16", resolved: true }], [19, { id: "t19", resolved: true, thumbsDown: 2 }]],
	  resolvedByUs: ["t15"], merged: true },
	{ pull: "github.com/o/r#2", now: "2026-10-02T00:00:00.000Z", findings: [finding("b1", "open"), finding("b2", "still_present", { commentId: 3 })],
	  threads: [[3, { id: "t3", resolved: true }]], resolvedByUs: ["t3"], merged: false },
	{ pull: "github.com/o/r#3", now: "2026-10-03T00:00:00.000Z", findings: [finding("c1", "still_present")], merged: true },
];
const fates = fateCases.map((c) => ({ input: c, fates: lessons.fatesOf({ ...c, statuses: c.statuses && new Map(c.statuses),
	threads: c.threads && new Map(c.threads), resolvedByUs: c.resolvedByUs && new Set(c.resolvedByUs) }) }));
const fate = (id, pull, at, category, kind, kindOfFate) => ({ id, pull, file: "x.ts", line: 1, category, kind, level: "low",
	claim: "c", fate: kindOfFate, how: "h", at });
const ledgerA = [];
for (let i = 0; i < 6; i++) ledgerA.push(fate(`r${i}`, "p#1", `2026-0${1 + (i % 9)}-15T10:00:00.000Z`, "correctness", "fast", i < 2 ? "accepted" : "rejected"));
for (let i = 0; i < 5; i++) ledgerA.push(fate(`s${i}`, "p#2", `2026-09-0${1 + i}T00:00:00Z`, "Tests", "deep:tests", i < 3 ? "accepted" : "rejected"));
for (let i = 0; i < 5; i++) ledgerA.push(fate(`u${i}`, "p#3", `2026-09-1${i}T00:00:00+02:00`, "arch", "deep:claims", "rejected"));
for (let i = 0; i < 4; i++) ledgerA.push(fate(`v${i}`, "p#4", "2026-09-20", "security", "fast", "rejected"));
ledgerA.push(fate("w0", "p#5", "not a date", "security", "fast", "rejected"));
ledgerA.push(fate("w1", "p#5", "2025-01-01T00:00:00Z", "security", "fast", "rejected"));
for (let i = 0; i < 5; i++) ledgerA.push(fate(`x${i}`, "p#6", `2026-08-0${1 + i}T12:30:45.5Z`, "ai", "compiled", i === 0 ? "accepted" : "rejected"));
for (let i = 0; i < 5; i++) ledgerA.push(fate(`y${i}`, "p#7", "2026-08-01T00:00:00Z", "ai", "Fast", "accepted"));
for (let i = 0; i < 6; i++) ledgerA.push(fate(`z${i}`, "p#8", "2026-09-01T00:00:00Z", "docs", "fast", i % 2 ? "accepted" : "rejected"));
// One fate exactly 180 days before the first case's `now`: inside the window.
for (let i = 0; i < 5; i++) ledgerA.push(fate(`e${i}`, "p#10", i ? "2026-09-01T00:00:00Z" : "2026-04-13T00:00:00.000Z", "edge", "fast", "accepted"));
const lessonCases = [
	{ ledger: ledgerA, now: "2026-10-10T00:00:00.000Z" }, { ledger: ledgerA, now: "2027-03-01T00:00:00Z" }, { ledger: [], now: "2026-10-10T00:00:00Z" },
	{ ledger: ledgerA, now: "2026-04-08T00:00:00Z" },
];
const lessonsOut = lessonCases.map((c) => ({ ...c, lessons: lessons.lessonsFor(c.ledger, c.now) }));
const many = Array.from({ length: 405 }, (_, i) => fate(`m${i}`, "p#9", `2026-01-01T00:${String(Math.floor(i / 60)).padStart(2, "0")}:${String(i % 60).padStart(2, "0")}Z`, "a", "fast", "accepted"));
const mergeCases = [
	{ ledger: ledgerA.slice(0, 6), fates: [fate("r1", "p#1", "2026-01-01T00:00:00Z", "correctness", "fast", "rejected"), fate("r1", "p#2", "2026-12-01T00:00:00Z", "x", "fast", "accepted")] },
	{ ledger: many.slice(0, 300), fates: many.slice(300).reverse() },
];
const merges = mergeCases.map((c) => ({ ...c, merged: lessons.mergeFates(c.ledger, c.fates) }));
writeFileSync(join(here, "ledger.json"), `${JSON.stringify({ fates, lessons: lessonsOut, merges }, null, 1)}\n`);

// Settings (autoreview/config.ts): resolveConfig, engineSettings, testsEligible, thinkingDefault, autoreviewPaths.
const config = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/config.ts")).href);
const logoModule = await import(pathToFileURL(join(root, "packages/coding-agent/src/experimental/ultron-logo.ts")).href);
const settingsCases = [
	[{}, {}],
	[{ ackArt: "none" }, { hasModel: ["claude-code/claude-haiku-5-5"] }],
	[{ ackArt: false, pollSeconds: 5, concurrency: 99, budget: 500, frameConcurrency: 0, deepRounds: 0, planCells: 40,
		verifyCandidates: -3, verifyBatch: 100, maxComments: 99, testRuns: 99, testTimeoutSeconds: 1, deadlineSeconds: 5,
		frameTimeoutSeconds: 3 }, { hasModel: ["cliproxyapi/gpt-6-luna"] }],
	[{ ackArt: " LOGO ", pollSeconds: 60.5, concurrency: 2.5, budget: 25_000, deadlineSeconds: 0, frameTimeoutSeconds: 0,
		modelConcurrency: { "a/b": 40, "c/d": 0, "e/f": 3.5 } },
	 { reviewModel: "r/m", rlm: { frameModel: "f/m" }, defaultProvider: "p", defaultModel: "d", hasModel: ["x/y"] }],
	[{ ackArt: "  my art  ", model: "cliproxyapi/gpt-6-luna", deepModel: "anthropic/claude-haiku-5-5", askModel: "o/x",
		thinking: "medium", accounts: ["me"], testOwners: ["ACME", "Me"], testEnv: { "o/r": "/env" }, testImage: "img",
		mise: "/opt/mise", checkoutRoots: ["/src"], guides: ["g.md"], dryRun: true, ack: false, ackLines: ["hi"],
		signature: false, learn: false, skipBots: false, runTests: false, prepareEnvs: false, systemPrefix: true,
		overlap: true, mode: "hybrid", planStyle: "frame", referenceView: "all", blockAt: "high",
		modelConcurrency: { "cliproxyapi/gpt-6-luna": 4 } }, { rlm: { frameModel: "f/m" } }],
	[{ ackArt: "", verifyModel: "v/m", planModel: "claude-code/claude-haiku-5-5", deadlineSeconds: 45,
		frameTimeoutSeconds: 15, budget: 0, mise: false }, { defaultProvider: "p" }],
	[{ ackArt: "none", systemPrefix: false, overlap: "yes" }, { defaultProvider: "p", defaultModel: "d" }],
];
const settings = settingsCases.map(([s, f]) => {
	const fallbacks = { ...f, hasModel: f.hasModel ? (ref) => f.hasModel.includes(ref) : undefined };
	const resolved = config.resolveConfig(s, fallbacks);
	return { settings: s, fallbacks: f, config: resolved, engine: config.engineSettings(resolved),
		eligible: [["o", true], ["O", false], ["acme", false], ["me", false]].map(([o, p]) => config.testsEligible(resolved, o, p)) };
});
const stages = ["finders", "verifier", "investigators", "planner", "asks"];
const thinking = [undefined, "cliproxyapi/gpt-6-luna", "opencode/claude-haiku-5-5", "x/y"].map((model) => ({
	model: model ?? null, levels: stages.map((stage) => config.thinkingDefault(stage, model)) }));
const paths = [
	["/agent", {}], ["/agent/", { ULTRON_AUTOREVIEW_CACHE_DIR: " /c " }], ["a", { ULTRON_AUTOREVIEW_CACHE_DIR: " ", XDG_CACHE_HOME: "/x" }],
	["/a/../b", { XDG_CACHE_HOME: "  " }],
].map(([agentDir, env]) => {
	// os.homedir() reads HOME: the recorded paths name a fixed home, never the generating user's.
	process.env.HOME = "/home/u";
	return { agentDir, env, paths: config.autoreviewPaths(agentDir, env) };
});
writeFileSync(join(here, "settings.json"), `${JSON.stringify({ logo: logoModule.logoText(), settings, thinking, paths }, null, 1)}\n`);

// Dependency discovery (autoreview/prepare.ts): detectEcosystems, toolchainRequests, environmentHash, findMise,
// describeToolchain, over repository trees built in a temp dir. Paths under it are recorded as `<root>`.
const prepare = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/prepare.ts")).href);
const { mkdtempSync, mkdirSync, rmSync, symlinkSync } = await import("node:fs");
const { tmpdir } = await import("node:os");
const file = (path, content = "") => ({ path, content });
const dir = (path) => ({ path, dir: true });
const link = (path, target) => ({ path, link: target });
const trees = [
	// The TS test's repository: services below the root, build/vendor/hidden directories and depth 5 left out.
	[file("pyproject.toml", '[project]\nname = "x"\nrequires-python = ">=3.11,<4"\n'), file("uv.lock", "version = 1\n"),
	 file("requirements/dev.txt", "pytest\n"), file("requirements/base.txt", "x\n"), file("requirements/notes.md"),
	 dir("requirements/sub.txt"), file("backend/requirements.txt", "fastapi\n"),
	 file("services/api/pyproject.toml", '[project]\nname = "api"\n'), file("services/api/deps/requirements-dev.txt", "pytest\n"),
	 file("a/b/c/d/e/requirements.txt", "too deep\n"), file("a/b/c/d/requirements-x.txt", "deep enough\n"),
	 file("frontend/dist/requirements.txt", "nope\n"), file("node_modules/pkg/requirements.txt", "nope\n"),
	 file(".tox/requirements.txt", "nope\n"), file("package.json", JSON.stringify({ name: "web", engines: { node: ">=20" } })),
	 file("pnpm-lock.yaml", "lockfileVersion: 9\n"), file(".nvmrc", "v22.13\n")],
	[file("mise.toml", '[env]\nX = "1"\n[tools]\npython = "3.12"\nnodejs = \'20\'\n\n[env]\nY = "2"\n'), file(".nvmrc", "18\n"),
	 file(".tool-versions", "python 3.9\nnode 16\n"), file("setup.py", "x"), file("requirements.txt", "a\n"),
	 file("package-lock.json", "{}"), file("yarn.lock", ""), file("svc/setup.py"), file("svc/setup.cfg"),
	 file("svc/requirements.TXT"), file("svc/requirementsXtxt"), file("svc/dev-requirements.txt"), file("Svc/requirements.txt"),
	 file("é/requirements.txt", "u"), file("z/requirements.txt"), file("_/pyproject.toml"), dir("docs/x"),
	 file("docs/requirements.txt"), file("build/requirements.txt"), file("vendor/pyproject.toml"), file("target/setup.py"),
	 file("coverage/setup.py"), file("site-packages/setup.py"), file("__pycache__/setup.py"), file("mockups/setup.py"),
	 file("venv/setup.py"), file("dist/setup.py"), file("x/requirements/requirements.txt"), file("x/.hidden/requirements.txt"),
	 file("x/.requirements.txt"), file("x/requirements.txt/inner.txt"), link("linked", "svc"), link("x/requirements-link.txt", "../z")],
	[file(".tool-versions", "  nodejs   21.1  \n\npython\nruby 3\npython 3.8.1 extra\n"), file(".python-version", "\n3.7\n3.6\n"),
	 file(".node-version", "v14\n"), file("package.json", '{"engines": {"node": "^19.1"}}'), file("mise.toml", "[tools]\nruby = '3'\n")],
	[file(".python-version", "  3.10  \nignored"), file(".nvmrc", ""), file(".node-version", "12"),
	 file("pyproject.toml", "requires-python='~=3.9'"), file("package.json", '{"engines": {"node": 18}}')],
	[file(".mise.toml", '[tools]\n  python = "3.11.4"\n'), file("pyproject.toml", 'requires-python = ">=3"\n'),
	 file("package.json", "not json"), file("requirements-dev.txt", "x")],
	[file("pyproject.toml", 'requires-python = ">=3.12"\n'), file("package.json", '{"engines": null}'), file("dev-requirements.txt", "")],
	[file("package.json", '{"engines": {"node": [16, 18]}}'), file("npm-shrinkwrap.json", "{}"), file("README.md", "x")],
	[file("package.json", '[1]'), file("mise.toml", "[tools]\nnode = \"lts\"")],
	// A falsy engines.node names no version (0 would otherwise read as "0").
	[file("package.json", '{"engines": {"node": 0}}')], [file("package.json", '{"engines": {"node": false}}')],
	[],
];
const build = (base, tree) => {
	for (const entry of tree) {
		const path = join(base, entry.path);
		mkdirSync(dirname(path), { recursive: true });
		if (entry.dir) mkdirSync(path, { recursive: true });
		else if (entry.link) symlinkSync(entry.link, path);
		else writeFileSync(path, entry.content);
	}
};
const repos = trees.map((tree) => {
	const base = mkdtempSync(join(tmpdir(), "review-prepare-"));
	try {
		build(base, tree);
		return { tree, detect: prepare.detectEcosystems(base), requests: prepare.toolchainRequests(base),
			hash: prepare.environmentHash(base), hashPython: prepare.environmentHash(base, "3.13") };
	} finally {
		rmSync(base, { recursive: true, force: true });
	}
});
const miseTree = [file("one/bin/mise"), file("two/mise"), dir("three/mise"), file("home/.local/bin/mise"), file("mise")];
const miseCases = [
	["<root>/none:<root>/one/bin:<root>/two", undefined], ["<root>/none:<root>/two/./:<root>/one/bin", true],
	["<root>/one/bin", false], ["<root>/one/bin", "  "], ["<root>/one/bin", " /opt/m "], ["<root>/one/bin", "true"],
	["<root>/three::.:", undefined], ["<root>/x/../two", undefined], ["", undefined], [undefined, "false"],
];
const miseBase = mkdtempSync(join(tmpdir(), "review-mise-"));
let mise;
try {
	build(miseBase, miseTree);
	const real = (text) => text?.replaceAll("<root>", miseBase);
	mise = miseCases.map(([path, setting]) => ({ path: path ?? null, setting: setting ?? null,
		found: prepare.findMise(path === undefined ? {} : { PATH: real(path) }, join(miseBase, "home"), setting)?.replaceAll(miseBase, "<root>") ?? null }));
} finally {
	rmSync(miseBase, { recursive: true, force: true });
}
const describe = prepare.describeToolchain([
	{ tool: "python", version: "3.12.9", path: "/m/python/3.12.9", source: "mise" },
	{ tool: "node", version: "20.0", path: "/m/node/20.0", source: "mise", installed: true },
	{ tool: "python", version: "", path: "/usr", source: "system" }, { tool: "node", version: "18", path: "/u", source: "uv", installed: false },
]);
writeFileSync(join(here, "prepare.json"), `${JSON.stringify({ repos, miseTree, mise, describe }, null, 1)}\n`);

// The posting plan (autoreview/plan.ts): verdict, ranking, inline comments, body; levels (types.ts).
const plan = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/plan.ts")).href);
const types = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/types.ts")).href);
const engineResult = (partial = {}) => ({
	complete: true, label: "o/r#1", files: 1, added: 1, removed: 1, findings: [], alsoRaised: [], earlier: [],
	dropped: { rejected: 0, duplicates: 0 }, timing: { totalMs: 42_000, scopeMs: 100, findMs: 30_000, verifyMs: 11_000 },
	usage: { inputTokens: 90_000, outputTokens: 4_000, costUsd: 0.31, frames: 12, tokens: 94_000, budget: 300_000 },
	model: "stub/frames", verifyModel: "stub/frames", notChecked: [], incomplete: [], diffLines: { "calc.py": [[1, 6]] },
	...partial,
});
const MAJOR = { file: "calc.py", line: 4, severity: "major", category: "correctness", claim: "total() skips the last item.",
	why: "range(len(items) - 1) stops one short.", suggestedFix: "Use range(len(items)).",
	replacement: "    for i in range(len(items)):", verification: "confirmed", confidence: 0.9 };
const fnd = (partial = {}) => ({ ...MAJOR, ...partial });
const earlier = (partial) => ({ id: "ccccccc-1", file: "calc.py", line: 4, claim: "x", severity: "major", evidence: "", ...partial });
const HEAD = "abcdef0123456789";
const long = "word ".repeat(120);
const planCases = [
	[engineResult(), {}],
	[engineResult({ findings: [fnd()] }), {}],
	[engineResult({ findings: [fnd({ level: "low" }), fnd({ level: "nit", line: 2 })] }), { signature: false }],
	[engineResult({ complete: false, incomplete: ["3 reviewer passes failed: a.py.", "timeout"], notChecked: ["timeout", "x.bin", "y", "z"] }), {}],
	[engineResult({ findings: [fnd()] }), { selfAuthored: true }],
	[engineResult({ findings: [fnd()] }), { state: "merged" }],
	[engineResult({ findings: [fnd({ level: "medium" })] }), { blockAt: "high" }],
	[engineResult({ findings: [fnd({ level: "low" })] }), { blockAt: "low" }],
	[engineResult({ findings: [fnd({ severity: "minor", replacement: undefined })] }), {}],
	[engineResult({ findings: [fnd({ level: "critical", verification: "uncertain" })] }), {}],
	[engineResult({ earlier: [earlier({ status: "still_present" }), earlier({ status: "fixed", file: "b.py", line: 9 }),
		earlier({ status: "unknown" }), earlier({ status: "not_applicable" }), earlier({ severity: "low", status: "still_present" })] }), {}],
	[engineResult({ earlier: [earlier({ severity: "low", status: "still_present" })] }), {}],
	// Many findings: ranking by tier, level and strength; the comment cap; relocation; alsoAt; summaries.
	[engineResult({
		files: 3, added: 120, removed: 7, dropped: { rejected: 2, duplicates: 1 },
		diffLines: { "calc.py": [[1, 6], [40, 45]], "b.py": [[10, 20]], "c.py": [[1, 3]] },
		findings: [
			fnd({ level: "low", line: 2, strength: "diff" }),
			fnd({ level: "medium", line: 42, strength: "outside", endLine: 44, replacement: "a\nb\nc" }),
			fnd({ level: "medium", line: 5, endLine: 9, replacement: "x ```` y" }),
			fnd({ level: "high", line: 22, file: "b.py", replacement: undefined, scenario: "  input [] gives 0;\n should raise  " }),
			fnd({ level: "critical", line: 100, alsoAt: [{ file: "c.py", line: 50 }, { file: "c.py", line: 2 }, { file: "z.py", line: 1 }] }),
			fnd({ level: "low", line: 3, strength: "test", lesson: "accepted 1 of 7 such findings in the last 180 days" }),
			fnd({ level: "nit", line: 1 }),
			fnd({ level: "low", line: 37, source: "deep:structure", replacement: undefined, suggestedFix: "" }),
			fnd({ level: "medium", file: "nowhere.py", line: 7, claim: long, why: long }),
			fnd({ level: "low", file: "nowhere.py", line: 8, claim: "outside one" }),
			fnd({ level: "low", file: "nowhere.py", line: 9, claim: "outside two" }),
			fnd({ level: "low", file: "nowhere.py", line: 10, claim: "outside three" }),
			fnd({ level: "low", file: "nowhere.py", line: 11, claim: "outside four?" }),
			fnd({ level: "high", line: 15, file: "b.py", verification: "uncertain" }),
			fnd({ level: "medium", line: 4, file: "c.py", alsoAt: [{ file: "a", line: 1 }, { file: "b", line: 2 }, { file: "c", line: 3 },
				{ file: "d", line: 4 }, { file: "e", line: 5 }, { file: "f", line: 6 }, { file: "g", line: 7 }] }),
		],
		alsoRaised: [{ file: "a", line: 1, severity: "x", claim: "c", by: ["al", "bo"] }, { file: "b", line: 2, severity: "x", claim: "d", by: ["bo", "cy", "di", "ed", "fy"] }],
		usage: { inputTokens: 1, outputTokens: 1, costUsd: 0, frames: 3, tokens: 999, budget: null },
		timing: { totalMs: 2250, scopeMs: 0, findMs: 0, verifyMs: 0 },
	}), { maxComments: 4 }],
	[engineResult({ findings: [fnd({ level: "high" }), fnd({ level: "high", line: 5 }), fnd({ level: "medium", line: 6 }), fnd({ level: "critical", line: 1 }),
		fnd({ level: "medium", line: 2 }), fnd({ level: "high", line: 3, strength: "test" })],
		assurance: ["The cache   key holds.", long, "c", "d", "e"], usage: { inputTokens: 1, outputTokens: 1, costUsd: 0.125, frames: 1, tokens: 1500, budget: null },
		timing: { totalMs: 9_950, scopeMs: 0, findMs: 0, verifyMs: 0 } }), { sinceSha: "0011223344", maxComments: 0 }],
	[engineResult({ findings: [fnd({ claim: "Weird\u0085space﻿ and café \u{1F600}" + "x".repeat(300), why: "", scenario: "   " })],
		usage: { inputTokens: 1, outputTokens: 1, costUsd: 1.005, frames: 1, tokens: 0, budget: null },
		timing: { totalMs: 10_500, scopeMs: 0, findMs: 0, verifyMs: 0 } }), { sinceSha: "0011223344", state: "closed" }],
	[engineResult({ findings: [fnd({ level: "low", file: "c.py", line: 40, alsoAt: [{ file: "calc.py", line: 50 }] }),
		fnd({ level: "low", line: 2, claim: "", why: "ends with colon:", suggestedFix: "fix it!" })],
		usage: { inputTokens: 1, outputTokens: 1, costUsd: 2.5, frames: 1, tokens: 2500, budget: null } }), { blockAt: "critical" }],
	// A cut that lands after a space: the kept text is trimmed before the ellipsis.
	[engineResult({ findings: [fnd({ claim: `${"a".repeat(218)} ${"b".repeat(40)}` })] }), {}],
];
const planOut = planCases.map(([result, extra]) => {
	const options = { selfAuthored: false, state: "open", headSha: HEAD, signature: true, ...extra };
	const p = plan.planReview(result, options);
	return { result, options, verdict: plan.decideVerdict(result, options), ranked: plan.rankFindings(result), plan: p,
		withoutInline: plan.withoutInline(p, result), comments: result.findings.map((f) => plan.commentText(f)),
		cost: plan.costLine(result) };
});
const ranges = [[10, 20], [40, 45]];
const places = [12, 22, 37, 30, 9, 6, 48, 49, 23, 24, 7, 42].map((line) => [line, plan.placeLine(ranges, line) ?? null]);
const levels = [{}, { severity: "blocker" }, { severity: "major" }, { severity: "minor" }, { severity: "nit" },
	{ severity: "medium" }, { level: "medium", severity: "minor" }, { level: "bogus", severity: "major" }, { level: "high", severity: "nit" },
	{ severity: "bogus" }].map((f) => [f, types.levelOf(f)]);
writeFileSync(join(here, "plan.json"), `${JSON.stringify({ plans: planOut, places, emptyPlace: plan.placeLine([], 3) ?? null,
	levels, severities: types.LEVELS.map(types.severityOf), marker: plan.commentMarker("k-1") }, null, 1)}\n`);
