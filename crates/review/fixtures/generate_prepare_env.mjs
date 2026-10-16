// Records `prepareEnvironment`, `resolveToolchain` (through it) and `cachedEnvironment` of
// packages/coding-agent/src/ultron/autoreview/prepare.ts over scripted scenarios: a repository and fake toolchain
// tree built in a temp dir, and a scripted runner (rules matched by the argv prefix, each answering its results in
// order, the last repeating, optionally creating directories) instead of uv, mise, npm, pnpm or yarn. Recorded per
// run: the returned object, every runner call (argv, cwd, env, timeout), the log lines, the toolchain.json text, the
// files of the environment directory, and cachedEnvironment afterwards. Paths under the temp dir read `<root>`, the
// environment hash `<hash>`, times are zeroed. Nothing runs a real process or touches the network.
// Usage: node crates/review/fixtures/generate_prepare_env.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const prepare = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview/prepare.ts")).href);

const file = (path, content = "") => ({ path, content });
const dir = (path) => ({ path, dir: true });
const link = (path, target) => ({ path, link: target });
const ok = (stdout = "", extra = {}) => ({ code: 0, stdout, stderr: "", ...extra });
const fail = (code, stderr, stdout = "") => ({ code, stdout, stderr });
const listing = (entries) => JSON.stringify(entries);

const LONG = `   ${"x".repeat(150)}é${"y".repeat(147)}😀${"z".repeat(40)}  \n`;

export const scenarios = [
	{
		name: "uv-lock-with-mise-python",
		tree: [
			file("repo/pyproject.toml", '[project]\nname = "x"\nrequires-python = ">=3.11"\n'),
			file("repo/uv.lock", "version = 1\n"),
			file("repo/mise.toml", '[tools]\npython = "3.12"\n'),
			file("bin/mise"), file("bin/uv"),
			file("installs/python/3.12.4/bin/python3"), file("installs/python/3.12.10/bin/python3"),
			file("installs/python/3.12.9/bin/python"), dir("installs/python/3.11.1"), dir("installs/python/3.12.99"),
			link("installs/python/latest", "<root>/installs/python/3.12.10"),
		],
		env: { PATH: "<root>/bin:/nonexistent" },
		rules: [
			{ match: "<root>/bin/mise ls --json", results: [ok(listing({
				python: [
					{ version: "3.12.4", install_path: "<root>/installs/python/3.12.4", installed: true },
					{ version: "3.12.10", install_path: "<root>/installs/python/latest" },
					{ version: "3.12.9", install_path: "<root>/installs/python/3.12.9", installed: true },
					{ version: "3.11.1", install_path: "<root>/installs/python/3.11.1", installed: true },
					{ version: "3.12.99", install_path: "<root>/installs/python/3.12.99", installed: false },
					{ version: "3.12.50", install_path: "<root>/installs/python/missing", installed: true },
					{ version: "3.120", install_path: "<root>/installs/python/3.12.4", installed: true },
				],
				node: [{ version: "20.1.0", install_path: "<root>/installs/python/3.12.4" }],
			}))] },
			{ match: "<root>/bin/uv venv", results: [ok()] },
			{ match: "<root>/bin/uv sync", results: [ok("Resolved 3 packages")] },
		],
		runs: [
			{ options: {} },
			// Cached: nothing runs.
			{ options: {} },
			// force: built again.
			{ options: { force: true, timeoutMs: 5000 } },
			// An explicit Python is another cache entry.
			{ options: { python: "3.12.4" } },
		],
	},
	{
		name: "editable-installs-and-requirements",
		tree: [
			file("repo/pyproject.toml", '[project]\nname = "x"\n'),
			file("repo/requirements.txt", "a\n"),
			file("repo/requirements/dev.txt", "b\n"),
			file("repo/services/api/pyproject.toml", "[project]\n"),
			file("repo/backend/setup.py", "x"),
			file("repo/.python-version", "3.10\n"),
			file("fakehome/.local/bin/uv"),
			link("sys/python3", "/usr/bin/env"),
		],
		env: { PATH: "<root>/empty:<root>/sys" },
		rules: [
			{ match: "<root>/fakehome/.local/bin/uv venv", results: [ok()] },
			{ match: "<root>/fakehome/.local/bin/uv pip install --python <root>/cache/envs/<hash>/.venv/bin/python -e <root>/repo[dev,test]", results: [fail(1, "no extra dev")] },
			{ match: "<root>/fakehome/.local/bin/uv pip install --python <root>/cache/envs/<hash>/.venv/bin/python -e <root>/repo[dev]", results: [fail(1, "no extra dev")] },
			{ match: "<root>/fakehome/.local/bin/uv pip install --python <root>/cache/envs/<hash>/.venv/bin/python -e <root>/repo[test]", results: [ok()] },
			{ match: "<root>/fakehome/.local/bin/uv pip install --python <root>/cache/envs/<hash>/.venv/bin/python -e <root>/repo/services/api", results: [fail(2, "", "  bad project  \n")] },
			{ match: "<root>/fakehome/.local/bin/uv pip install --python <root>/cache/envs/<hash>/.venv/bin/python -e <root>/repo/backend[dev,test]", results: [ok()] },
			{ match: "<root>/fakehome/.local/bin/uv pip install --python <root>/cache/envs/<hash>/.venv/bin/python -r <root>/repo/requirements.txt", results: [ok()] },
			{ match: "<root>/fakehome/.local/bin/uv pip install --python <root>/cache/envs/<hash>/.venv/bin/python -r <root>/repo/requirements/dev.txt", results: [fail(1, LONG)] },
		],
		runs: [
			{ options: { mise: false } },
			// The record says it failed: not a cache hit, built again.
			{ options: { mise: false } },
		],
	},
	{
		name: "node-pnpm-with-mise-install",
		tree: [
			file("repo/package.json", '{"name":"web","engines":{"node":">=20"}}'),
			file("repo/pnpm-lock.yaml", "lockfileVersion: 9\n"),
			file("repo/yarn.lock", ""),
			file("repo/.npmrc", "ignore-scripts=true\n"),
		],
		env: { PATH: "/nonexistent" },
		rules: [
			{ match: "<root>/m/mise ls", results: [ok(listing({ node: [] })), ok(listing({ node: [{ version: "20.0.0", install_path: "<root>/installs/node/20.0.0", installed: true }] }))] },
			{ match: "<root>/m/mise install node@20", results: [ok("installed", { mkdirs: ["<root>/installs/node/20.0.0/bin"] })] },
			{ match: "pnpm install", results: [ok()] },
		],
		runs: [{ options: { mise: "<root>/m/mise", timeoutMs: 7000 } }],
	},
	{
		name: "everything-fails",
		tree: [
			file("repo/setup.cfg", "[metadata]\n"),
			file("repo/package.json", "{}"),
			file("repo/.tool-versions", "python 3.11\n"),
			file("bin/mise"),
			file("opt/py/bin/python3"), dir("opt/py/lib"),
			file("opt/nolib/bin/node"),
			link("managed/python3", "<root>/opt/py/bin/python3"),
			link("managed/node", "<root>/opt/nolib/bin/node"),
		],
		env: { PATH: "<root>/bin:<root>/managed" },
		rules: [
			{ match: "<root>/bin/mise ls", results: [fail(1, "mise: no config")] },
			{ match: "<root>/bin/mise install", results: [fail(1, `  boom ${"!".repeat(250)}`)] },
		],
		runs: [{ options: {} }],
	},
	{
		name: "venv-without-dependency-list",
		tree: [
			file("repo/setup.cfg", "[metadata]\n"),
			file("repo/.python-version", "3.9\n"),
			file("bin/uv"),
		],
		env: { PATH: "<root>/bin" },
		rules: [{ match: "<root>/bin/uv venv", results: [ok()] }],
		runs: [{ options: { mise: false } }],
	},
	{
		name: "mise-listing-unreadable",
		tree: [
			file("repo/requirements.txt", "a\n"),
			file("repo/.tool-versions", "python ^3.12\n"),
			file("bin/mise"), file("bin/uv"),
			link("sys/python3", "/usr/bin/env"),
		],
		env: { PATH: "<root>/bin:<root>/sys" },
		rules: [
			{ match: "<root>/bin/mise ls", results: [ok("not json"), ok("null")] },
			{ match: "<root>/bin/mise install python@^3.12", results: [ok()] },
			{ match: "<root>/bin/uv venv", results: [fail(2, "", `out msg ${LONG}`)] },
		],
		runs: [{ options: {} }],
	},
	{
		name: "uv-sync-and-npm-ci-fail",
		tree: [
			file("repo/pyproject.toml", '[project]\nname = "x"\n'),
			file("repo/uv.lock", "version = 1\n"),
			file("repo/package.json", "{}"),
			file("repo/package-lock.json", "{}"),
			file("bin/uv"),
			link("bin/python3", "/usr/bin/env"),
			link("bin/node", "/usr/bin/env"),
		],
		env: { PATH: "<root>/bin" },
		rules: [
			{ match: "<root>/bin/uv venv", results: [ok()] },
			{ match: "<root>/bin/uv sync", results: [fail(1, LONG)] },
			{ match: "npm ci", results: [fail(1, "", LONG)] },
		],
		runs: [{ options: { mise: false } }],
	},
	{
		name: "yarn-failure-and-explicit-python",
		tree: [file("repo/package.json", '{"name":"web"}'), file("repo/yarn.lock", "")],
		env: { PATH: "/nonexistent" },
		rules: [{ match: "yarn install", results: [fail(1, "", "yarn out")] }],
		runs: [{ options: { mise: false, python: "3.13" } }],
	},
	{
		name: "cache-records",
		tree: [
			file("repo/package.json", '{"name":"web"}'),
			file("repo/package-lock.json", "{}"),
			file("repo/npm-shrinkwrap.json", "{}"),
			link("sys/node", "/usr/bin/env"),
		],
		env: { PATH: "<root>/sys" },
		rules: [{ match: "npm ci", results: [ok()] }],
		runs: [
			{ options: { mise: false }, before: [{ record: "not json" }] },
			{ options: { mise: false } },
			{ options: { mise: false }, before: [{ record: '{"ok": 1, "dir": "/elsewhere", "hash": "h", "prepared": ["x"], "cached": false, "toolchain": [], "failures": [], "ms": 5, "extra": true}' }] },
			{ options: { mise: false }, before: [{ record: "null" }] },
			{ options: { mise: false }, before: [{ record: '{"ok": 0, "prepared": ["old"]}' }] },
		],
	},
];

const build = (base, tree) => {
	for (const entry of tree) {
		const path = join(base, entry.path);
		mkdirSync(dirname(path), { recursive: true });
		if (entry.dir) mkdirSync(path, { recursive: true });
		else if (entry.link) symlinkSync(entry.link.replaceAll("<root>", base), path);
		else writeFileSync(path, entry.content);
	}
};

async function runScenario(scenario) {
	const base = realpathSync(mkdtempSync(join(tmpdir(), "review-prepare-env-")));
	try {
		build(base, scenario.tree);
		const repo = join(base, "repo");
		mkdirSync(repo, { recursive: true });
		const cache = join(base, "cache");
		const real = (text) => text.replaceAll("<root>", base);
		const env = Object.fromEntries(Object.entries(scenario.env).map(([key, value]) => [key, real(value)]));
		const used = new Map();
		const runs = [];
		for (const run of scenario.runs) {
			const python = run.options.python;
			const hash = prepare.environmentHash(repo, python);
			const norm = (text) => text.replaceAll(base, "<root>").replaceAll(hash, "<hash>");
			for (const step of run.before ?? []) {
				mkdirSync(join(cache, "envs", hash), { recursive: true });
				writeFileSync(join(cache, "envs", hash, "toolchain.json"), step.record);
			}
			const calls = [];
			const logs = [];
			const runner = async (argv, options = {}) => {
				const joined = norm(argv.join(" "));
				calls.push({
					argv: argv.map(norm),
					cwd: options.cwd === undefined ? null : norm(options.cwd),
					env: options.env === undefined ? null : JSON.parse(norm(JSON.stringify(options.env))),
					timeoutMs: options.timeoutMs ?? null,
				});
				const index = scenario.rules.findIndex((rule) => joined.startsWith(rule.match));
				if (index < 0) return { code: 127, stdout: "", stderr: `${argv[0]}: not scripted` };
				const rule = scenario.rules[index];
				const count = used.get(index) ?? 0;
				used.set(index, count + 1);
				const result = rule.results[Math.min(count, rule.results.length - 1)];
				for (const made of result.mkdirs ?? []) mkdirSync(real(made), { recursive: true });
				return { code: result.code, stdout: real(result.stdout), stderr: real(result.stderr) };
			};
			const options = {
				runner,
				cacheDir: cache,
				env,
				home: join(base, "fakehome"),
				log: (line) => logs.push(norm(line)),
				...(run.options.python === undefined ? {} : { python: run.options.python }),
				...(run.options.mise === undefined
					? {}
					: { mise: typeof run.options.mise === "string" ? real(run.options.mise) : run.options.mise }),
				...(run.options.force === undefined ? {} : { force: run.options.force }),
				...(run.options.timeoutMs === undefined ? {} : { timeoutMs: run.options.timeoutMs }),
			};
			let result = null;
			let error = null;
			try {
				result = await prepare.prepareEnvironment(repo, options);
			} catch (caught) {
				error = String(caught.message);
			}
			let record = null;
			try {
				record = norm(readFileSync(join(cache, "envs", hash, "toolchain.json"), "utf8"))
					.replace(/"ms": \d+/, '"ms": 0')
					.replace(/"at": "[^"]*"/, '"at": "<at>"');
			} catch {}
			let files = [];
			try {
				files = readdirSync(join(cache, "envs", hash)).sort();
			} catch {}
			const zero = (value) =>
				value === undefined || value === null
					? null
					: JSON.parse(
							norm(
								JSON.stringify({
									...value,
									...("ms" in value ? { ms: 0 } : {}),
									...("at" in value ? { at: "<at>" } : {}),
								}),
							),
						);
			runs.push({
				options: run.options,
				before: run.before ?? [],
				result: zero(result),
				error,
				calls,
				logs,
				record,
				files,
				cached: zero(prepare.cachedEnvironment(repo, cache, python)),
			});
		}
		return runs;
	} finally {
		rmSync(base, { recursive: true, force: true });
	}
}

const recorded = [];
for (const scenario of scenarios) {
	recorded.push({ name: scenario.name, tree: scenario.tree, env: scenario.env, rules: scenario.rules, runs: await runScenario(scenario) });
}
writeFileSync(join(here, "prepare_env.json"), `${JSON.stringify({ scenarios: recorded }, null, 1)}\n`);
