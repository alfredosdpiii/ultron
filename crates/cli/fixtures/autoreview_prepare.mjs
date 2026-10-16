// Records the TypeScript `ultron autoreview prepare <dir> [--python 3.x] [--json]` and `autoreview review --repo-dir
// ... --prepare` (runAutoreviewCommand of packages/coding-agent/src/ultron/autoreview/cli.ts, in-process) with a
// scripted runner instead of uv, npm and git, and a fake engine that records the spec it is given. Recorded per case:
// the exit code, stdout, stderr, every runner call's argv, and the engine specs. Paths under the temp dir read
// `<root>`, environment hashes `<hash>`, times are zeroed. No process runs, no network, no model.
// Usage: node --import ./packages/coding-agent/src/experimental/source-resolver.ts crates/cli/fixtures/autoreview_prepare.mjs
import { guardNodeOptions } from "./offline-guard.mjs";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

void guardNodeOptions;
const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const cli = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/cli.ts")).href);
const prepare = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/prepare.ts")).href);

const ok = (stdout = "") => ({ code: 0, stdout, stderr: "" });
const fail = (code, stderr) => ({ code, stdout: "", stderr });
const RESULT = {
	complete: true, label: "local", files: 1, added: 1, removed: 1, findings: [], alsoRaised: [], earlier: [],
	dropped: { rejected: 0, duplicates: 0 }, timing: { totalMs: 42000, scopeMs: 100, findMs: 30000, verifyMs: 11000 },
	usage: { inputTokens: 900, outputTokens: 40, costUsd: 0.01, frames: 2, tokens: 940, budget: 300000 },
	model: "stub/frames", verifyModel: "stub/frames", notChecked: [], incomplete: [], diffLines: {},
};

// Every case runs in a fresh root: `repo` (requirements.txt), `bin/uv`, `sys/python3` -> /usr/bin/env (a system
// Python), `agent/settings.json` with autoreview.mise off, `checkouts/r/.git` (a local checkout of o/r).
export const cases = [
	{ name: "prepare-needs-a-directory", args: ["prepare"], rules: [] },
	{ name: "prepare-text", args: ["prepare", "repo", "--python", "3.12"], rules: [{ match: "<root>/bin/uv", results: [ok()] }] },
	{ name: "prepare-json", args: ["prepare", "repo", "--json"], rules: [{ match: "<root>/bin/uv", results: [ok()] }] },
	{
		name: "prepare-failure",
		args: ["prepare", "./repo/"],
		rules: [{ match: "<root>/bin/uv venv", results: [ok()] }, { match: "<root>/bin/uv pip", results: [fail(1, "no index\n")] }],
	},
	{
		name: "review-prepare-twice",
		args: ["review", "--repo-dir", "repo", "--base", "b", "--head", "h", "--prepare", "--json"],
		repeat: 2,
		rules: [{ match: "<root>/bin/uv", results: [ok()] }],
	},
	{
		name: "review-prepare-failed",
		args: ["review", "--repo-dir", "repo", "--base", "b", "--head", "h", "--prepare", "--python", "3.11"],
		rules: [{ match: "<root>/bin/uv venv", results: [fail(2, "no interpreter")] }],
	},
	{
		name: "review-prepare-test-env-wins",
		args: ["review", "--repo-dir", "repo", "--base", "b", "--head", "h", "--prepare", "--test-env", "venv", "--json"],
		rules: [{ match: "<root>/bin/uv", results: [ok()] }],
	},
	{
		name: "review-lent-checkout",
		args: ["review", "--repo-dir", "repo", "--base", "b", "--head", "h", "--repo", "o/r", "--checkout-roots", "<root>/checkouts", "--json"],
		rules: [{ match: "git -C <root>/checkouts/r config", results: [ok("remote.origin.url https://github.com/o/r.git\n")] }],
	},
	{
		name: "review-lent-checkout-prepared",
		args: ["review", "--repo-dir", "repo", "--base", "b", "--head", "h", "--repo", "o/r", "--checkout-roots", "<root>/checkouts", "--prepare", "--json"],
		rules: [
			{ match: "git -C <root>/checkouts/r config", results: [ok("remote.origin.url https://github.com/o/r.git\n")] },
			{ match: "<root>/bin/uv", results: [ok()] },
		],
	},
];

async function runCase(entry) {
	const base = realpathSync(mkdtempSync(join(tmpdir(), "cli-autoreview-prepare-")));
	try {
		mkdirSync(join(base, "repo"), { recursive: true });
		writeFileSync(join(base, "repo", "requirements.txt"), "a\n");
		mkdirSync(join(base, "bin"), { recursive: true });
		writeFileSync(join(base, "bin", "uv"), "");
		mkdirSync(join(base, "sys"), { recursive: true });
		symlinkSync("/usr/bin/env", join(base, "sys", "python3"));
		mkdirSync(join(base, "agent"), { recursive: true });
		writeFileSync(join(base, "agent", "settings.json"), JSON.stringify({ autoreview: { mise: false } }));
		mkdirSync(join(base, "checkouts", "r", ".git"), { recursive: true });
		mkdirSync(join(base, "fakehome"), { recursive: true });
		// prepareEnvironment looks in ~/.local/bin for uv: the fake home, never the generating user's.
		process.env.HOME = join(base, "fakehome");
		const hashes = [undefined, "3.11", "3.12"].map((python) => prepare.environmentHash(join(base, "repo"), python));
		const norm = (text) => hashes.reduce((out, hash) => out.replaceAll(hash, "<hash>"), text.replaceAll(base, "<root>"));
		const used = new Map();
		const calls = [];
		const runner = async (argv) => {
			const joined = norm(argv.join(" "));
			calls.push(argv.map(norm));
			const index = entry.rules.findIndex((rule) => joined.startsWith(rule.match));
			if (index < 0) return { code: 127, stdout: "", stderr: `${argv[0]}: not scripted` };
			const count = used.get(index) ?? 0;
			used.set(index, count + 1);
			const results = entry.rules[index].results;
			return results[Math.min(count, results.length - 1)];
		};
		const specs = [];
		const runs = [];
		for (let round = 0; round < (entry.repeat ?? 1); round++) {
			let stdout = "";
			let stderr = "";
			const code = await cli.runAutoreviewCommand(entry.args.map((arg) => arg.replaceAll("<root>", base)), {
				agentDir: join(base, "agent"),
				cwd: base,
				env: { PATH: `${base}/bin:${base}/sys`, ULTRON_AUTOREVIEW_CACHE_DIR: join(base, "cache"), ULTRON_HINDSIGHT_URL: "off" },
				runner,
				engine: () => ({
					close: async () => {},
					review: async (spec) => {
						specs.push(JSON.parse(norm(JSON.stringify(spec))));
						return RESULT;
					},
				}),
				io: { stdout: (text) => { stdout += text; }, stderr: (text) => { stderr += text; } },
				home: join(base, "fakehome"),
			});
			const zeroed = norm(stdout).replace(/"ms":\d+/g, '"ms":0').replace(/"at":"[^"]*"/g, '"at":"<at>"').replace(/^ {2}\d+ s$/m, "  0 s");
			runs.push({ code, stdout: zeroed, stderr: norm(stderr) });
		}
		return { name: entry.name, args: entry.args, rules: entry.rules, runs, calls, specs };
	} finally {
		rmSync(base, { recursive: true, force: true });
	}
}

const recorded = [];
for (const entry of cases) recorded.push(await runCase(entry));
writeFileSync(join(here, "autoreview_prepare.json"), `${JSON.stringify({ cases: recorded }, null, 1)}\n`);
