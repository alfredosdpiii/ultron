// Records loki.py's `init` (without --minimal) for the Rust differential test (tests/init_differential.rs).
// Run: node crates/loki/fixtures/generate-init.mjs  (writes init-corpus.json next to this file)
// Python: $ULTRON_PYTHON, else /usr/bin/python3. Deterministic: run twice and compare sha256.
//
// The engine source root loki.py installs from is assembled in a temp dir: the vendored engine
// (packages/coding-agent/src/ultron/loki-engine) plus the init assets of the same upstream commit
// (crates/loki/init-assets: templates, shim, vendor/anti-slop). Python runs with a scrubbed environment
// (PATH = a private bin dir, HOME = a private dir); no network is involved.
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
	chmodSync,
	cpSync,
	mkdirSync,
	mkdtempSync,
	readdirSync,
	readFileSync,
	realpathSync,
	rmSync,
	statSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const engineDir = join(here, "../../../packages/coding-agent/src/ultron/loki-engine");
const assets = join(here, "../init-assets");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const pythonReal = realpathSync(python);

const scratch = realpathSync(mkdtempSync(join(tmpdir(), "loki-init-gen-")));
const source = join(scratch, "src");
mkdirSync(source);
cpSync(join(engineDir, "loki.py"), join(source, "loki.py"));
cpSync(join(engineDir, "templates"), join(source, "templates"), { recursive: true });
for (const part of ["templates", "shim", "vendor"]) cpSync(join(assets, part), join(source, part), { recursive: true });

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
/** Installed files identical to a source file are recorded as `{ same: "<source path>" }`. */
const sources = new Map();
const index = (current) => {
	for (const name of readdirSync(current).sort()) {
		const path = join(current, name);
		if (statSync(path).isDirectory()) index(path);
		else if (!sources.has(sha256(readFileSync(path)))) sources.set(sha256(readFileSync(path)), relative(source, path));
	}
};
index(source);
/** The engine (large, fixed) is recorded by digest; the rest as text. */
const LARGE = 65536;

function tree(dir) {
	const out = {};
	const walk = (current) => {
		for (const name of readdirSync(current).sort()) {
			const path = join(current, name);
			const info = statSync(path);
			if (info.isDirectory()) walk(path);
			else {
				const bytes = readFileSync(path);
				const key = relative(dir, path);
				const same = sources.get(sha256(bytes));
				out[key] =
					same !== undefined
						? { same }
						: bytes.length > LARGE
							? { sha256: sha256(bytes), size: bytes.length }
							: bytes.toString("utf8");
				if ((info.mode & 0o777) === 0o444) out[key] = { readonly: true, ...(typeof out[key] === "string" ? { text: out[key] } : out[key]) };
			}
		}
	};
	walk(dir);
	return out;
}

const settings = (hooks, extra = {}) => JSON.stringify({ ...extra, hooks }, null, 2);
const staleLoki = {
	matcher: "Edit|Write",
	hooks: [
		{ type: "command", command: "python3 .loki/loki.py protect --harness claude" },
		{ type: "command", command: "echo keep-me" },
	],
};
const onlyLoki = { matcher: "Write", hooks: [{ type: "command", command: "python3 /x/.loki/loki.py hook" }] };

const cases = [
	{ name: "empty-repo", runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "verify-uses-loki-on-path", lokiOnPath: true, runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "nested-missing-dir", runs: [["init", "--dir", "<ROOT>/a/b"]] },
	{ name: "relative-dir-default", runs: [["init"]] },
	{ name: "rerun-is-idempotent", runs: [["init", "--dir", "<ROOT>"], ["init", "--dir", "<ROOT>"]] },
	{
		name: "merges-existing-hook-configs",
		files: {
			".claude/settings.json": settings(
				{ PreToolUse: [staleLoki, "plain", { matcher: "Bash" }], Stop: [onlyLoki] },
				{ model: "x", "permissions": { allow: ["Bash(ls)"] } },
			),
			".codex/hooks.json": settings({ PostToolUse: [onlyLoki, { hooks: "not-a-list" }] }),
			".factory/hooks.json": JSON.stringify({ PreToolUse: [onlyLoki], Custom: [] }),
			".loki/loki.json": '{"rule_packs": ["core"]}\n',
		},
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{
		name: "existing-linter-configs-are-skipped",
		files: {
			".ruff.toml": "line-length = 100\n",
			".golangci.yml": "version: \"2\"\n",
			".github/workflows/loki.yml": "name: mine\n",
			// Literal text: JSON.stringify writes 1e20 as an integer literal, which Python keeps as an int and the
			// Rust engine reads as a float (integers beyond 64 bits: see DECISIONS).
			".oxlintrc.json":
				'{"plugins": ["typescript", "react"], "ignorePatterns": ["dist/**"], ' +
				'"rules": {"no-empty": "off", "eqeqeq": "error"}, "categories": {"style": "warn"}, ' +
				'"extra": 1.5, "big": 1e20, "tiny": 0.00001, "count": 12, "neg": -0.5, "name": "café \u{1F600}"}',
		},
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{ name: "oxlint-config-ts-skipped", files: { "oxlint.config.ts": "export default {};\n" }, runs: [["init", "--dir", "<ROOT>"]] },
	{
		name: "oxlint-json-wins-over-config-ts",
		files: { "oxlint.config.ts": "export default {};\n", ".oxlintrc.json": "{}" },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{ name: "elixir-gets-credo", files: { "mix.exs": "defmodule X do end\n" }, runs: [["init", "--dir", "<ROOT>"]] },
	{
		name: "elixir-existing-credo-skipped",
		files: { "mix.exs": "x\n", ".credo.exs": "%{}\n" },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{ name: "npm-project", files: { "package.json": "{}" }, runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "bun-project", files: { "package.json": "{}", "bun.lockb": "" }, runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "bun-text-lock", files: { "package.json": "{}", "bun.lock": "" }, runs: [["init", "--dir", "<ROOT>"]] },
	{
		name: "pnpm-project",
		files: { "package.json": "{}", "pnpm-lock.yaml": "", "yarn.lock": "" },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{ name: "yarn-project", files: { "package.json": "{}", "yarn.lock": "" }, runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "codex-config-appends-features", files: { ".codex/config.toml": 'model = "o3"' }, runs: [["init", "--dir", "<ROOT>"]] },
	{
		name: "codex-features-table-gets-hooks",
		files: { ".codex/config.toml": "[features]  # flags\nweb = true\n\n[other]\nx = 1\n" },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{
		name: "codex-features-at-end-without-newline",
		files: { ".codex/config.toml": "a = 1\n[features]" },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{ name: "codex-hooks-already-true", files: { ".codex/config.toml": "[features]\nhooks = true\n" }, runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "codex-hooks-false-skipped", files: { ".codex/config.toml": "[features]\nhooks = false\n" }, runs: [["init", "--dir", "<ROOT>"]] },
	{
		name: "codex-inline-features-skipped",
		files: { ".codex/config.toml": "features = { web = true }\n" },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{
		name: "codex-dotted-features-skipped",
		files: { ".codex/config.toml": "features.web = true\n" },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{
		name: "codex-subtable-only-skipped",
		files: { ".codex/config.toml": "[features.sub]\nx = 1\n" },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{ name: "codex-features-not-table", files: { ".codex/config.toml": "features = 3\n" }, runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "invalid-json-settings", files: { ".claude/settings.json": "{\"hooks\": " }, runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "settings-not-object", files: { ".claude/settings.json": "[1]" }, runs: [["init", "--dir", "<ROOT>"]] },
	{ name: "hooks-not-object", files: { ".claude/settings.json": '{"hooks": []}' }, runs: [["init", "--dir", "<ROOT>"]] },
	{
		name: "event-not-array",
		files: { ".claude/settings.json": '{"hooks": {"PreToolUse": {}}}' },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{
		name: "factory-event-not-array",
		files: { ".factory/hooks.json": '{"SessionStart": "x"}' },
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{
		name: "existing-engine-kept-without-force",
		files: {
			".loki/loki.py": "# old engine\n",
			".loki/oxlint/anti-slop/index.ts": "// old\n",
			".pi/extensions/loki.ts": "// old shim\n",
		},
		runs: [["init", "--dir", "<ROOT>"]],
	},
	{
		name: "force-replaces-engine-and-plugin",
		files: { ".loki/loki.py": "# old engine\n", ".loki/oxlint/anti-slop/stale.ts": "// stale\n" },
		runs: [["init", "--force", "--dir", "<ROOT>"]],
	},
	{ name: "shell-guard", runs: [["init", "--shell-guard", "--dir", "<ROOT>"]] },
	{
		name: "shell-guard-replaces-previous",
		runs: [
			["init", "--shell-guard", "--dir", "<ROOT>"],
			["init", "--shell-guard", "--dir", "<ROOT>"],
		],
	},
	{
		// An earlier shell guard whose engine path is not `.loki/loki.py` survives the hook merge; the shell guard
		// replaces it (matcher Bash/Execute with a ` shell --harness ` handler) and keeps other Bash entries.
		name: "shell-guard-replaces-foreign-shell-entry",
		files: {
			".claude/settings.json": settings({
				PreToolUse: [
					{ matcher: "Bash", hooks: [{ type: "command", command: "/opt/engine.py shell --harness claude" }] },
					{ matcher: "Bash", hooks: [{ type: "command", command: "echo audit" }] },
					{ matcher: "Edit", hooks: [{ type: "command", command: "/opt/engine.py shell --harness claude" }] },
				],
			}),
			".factory/hooks.json": JSON.stringify({
				PreToolUse: [{ matcher: "Execute", hooks: [{ type: "command", command: "x shell --harness factory" }] }],
			}),
		},
		runs: [["init", "--shell-guard", "--dir", "<ROOT>"]],
	},
	{ name: "managed-engine", runs:[["init", "--managed-dir", "<MANAGED>", "--dir", "<ROOT>"]] },
	{ name: "managed-with-shell-guard", runs: [["init", "--shell-guard", "--managed-dir", "<MANAGED>", "--dir", "<ROOT>"]] },
	{ name: "managed-inside-repo", runs: [["init", "--managed-dir", "<ROOT>/managed", "--dir", "<ROOT>"]] },
	{
		name: "managed-release-mismatch",
		managedFiles: { "e5c62e282c776c92c076942011e3154b9c890560cacf323704e3d822b4dc3b6b/loki.py": "tampered\n" },
		runs: [["init", "--managed-dir", "<MANAGED>", "--dir", "<ROOT>"]],
	},
	{
		name: "managed-unrecognized-hook",
		files: {
			".claude/settings.json": settings({
				Stop: [{ hooks: [{ type: "command", command: "python3 .loki/loki.py explain" }] }],
			}),
		},
		runs: [["init", "--managed-dir", "<MANAGED>", "--dir", "<ROOT>"]],
	},
	{ name: "minimal-with-shell-guard-refused", runs: [["init", "--minimal", "--shell-guard", "--dir", "<ROOT>"]] },
	{ name: "settings-is-directory", dirs: [".claude/settings.json"], runs: [["init", "--dir", "<ROOT>"]] },
];

const guardLog = join(scratch, "guard.log");
const results = [];
for (const testCase of cases) {
	const work = join(scratch, "w");
	rmSync(work, { recursive: true, force: true });
	mkdirSync(work);
	const root = join(work, "repo");
	const managed = join(work, "managed");
	const home = join(work, "home");
	const bin = join(work, "bin");
	for (const dir of [root, managed, home, bin]) mkdirSync(dir);
	if (testCase.lokiOnPath) {
		writeFileSync(join(bin, "loki"), "#!/bin/sh\nexit 0\n");
		chmodSync(join(bin, "loki"), 0o755);
	}
	for (const [path, text] of Object.entries(testCase.files ?? {})) {
		mkdirSync(dirname(join(root, path)), { recursive: true });
		writeFileSync(join(root, path), text);
	}
	for (const path of testCase.dirs ?? []) mkdirSync(join(root, path), { recursive: true });
	for (const [path, text] of Object.entries(testCase.managedFiles ?? {})) {
		mkdirSync(dirname(join(managed, path)), { recursive: true });
		writeFileSync(join(managed, path), text);
	}
	const fill = (arg) => arg.replaceAll("<ROOT>", root).replaceAll("<MANAGED>", managed);
	const normalize = (text) =>
		text
			.replaceAll(`${source}/`, "")
			.replaceAll(root, "<ROOT>")
			.replaceAll(managed, "<MANAGED>")
			.replaceAll(pythonReal, "<PYTHON>")
			.replaceAll(python, "<PYTHON>");
	const runs = [];
	for (const args of testCase.runs) {
		const result = spawnSync(python, [join(source, "loki.py"), ...args.map(fill)], {
			cwd: root,
			env: { PATH: bin, HOME: home, LANG: "C.UTF-8", ULTRON_GUARD_LOG: guardLog },
			encoding: "utf8",
		});
		if (result.error) throw result.error;
		runs.push({
			args,
			status: result.status,
			stdout: normalize(result.stdout),
			stderr: normalize(result.stderr),
		});
	}
	const normalizeTree = (files) =>
		Object.fromEntries(
			Object.entries(files).map(([path, value]) => [
				path,
				typeof value === "string"
					? normalize(value)
					: value.text === undefined
						? value
						: { ...value, text: normalize(value.text) },
			]),
		);
	results.push({
		name: testCase.name,
		files: testCase.files ?? {},
		dirs: testCase.dirs ?? [],
		managedFiles: testCase.managedFiles ?? {},
		lokiOnPath: testCase.lokiOnPath === true,
		runs,
		tree: normalizeTree(tree(root)),
		managed: normalizeTree(tree(managed)),
	});
}
rmSync(scratch, { recursive: true, force: true });
assertOffline();
writeFileSync(
	join(here, "init-corpus.json"),
	`${JSON.stringify({ engine: "loki.py 0.1.4 (8adfc66)", cases: results }, null, "\t")}\n`,
);
console.log(`init-corpus.json: ${results.length} cases`);
