// Records loki.py's behavior over a corpus of projects for the Rust differential tests.
// Run: node crates/loki/fixtures/generate.mjs  (writes protect-corpus.json next to this file)
// Python: $ULTRON_PYTHON, else /usr/bin/python3. Deterministic: run twice and compare sha256.
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const engine = join(here, "../../../packages/coding-agent/src/ultron/loki-engine/loki.py");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";

// Credential-shaped tokens, stored split so neither this file nor the fixture matches a secret rule.
const tokens = {
	AWS: ["AKIA", "Q7XRJ2M4PL3ZWNVB"],
	AWS2: ["ASIA", "J4K8M2N6P1Q9R3S7"],
	GH: ["ghp_", "a8Kd93LmQz7Xw2Pv5Rt1Yb6Nc4Hj0Ge9Fs3D"],
	PEM: ["-----BEGIN RSA PRIV", "ATE KEY-----"],
	SLACK: ["xoxb-", "8273645190-Zq7Wm2Kx9Lp4Rt6Vn"],
	STRIPE: ["sk_live_", "4eC39HqLyjWDarjtT1zdp7dc"],
	NPM: ["npm_", "Ab3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dF3hJ5"],
	GOOGLE: ["AIza", "SyD3k9Lm2Qp7Xr4Tv8Wz1Bc5Nf6Hj0Gk2Ms"],
	MODEL: ["sk-ant-", "api03-Xk9mQ2vL7pR4tW8zB1nC5fH3jD6gK0sY2uE4"],
	FAKE: ["AKIA", "EXAMPLEXAMPLEXAMPL"],
	LOW: ["AKIA", "AAAAAAAAAAAAAAAA"],
	MID: ["AKIA", "ABABABABCDCDCDCD"],
};
const expand = (text) => text.replace(/@@(\w+)@@/g, (_, name) => tokens[name].join(""));

const write = (tool_name, content) => JSON.stringify({ tool_name, tool_input: { path: "x", content } });
const piEdit = (oldText, newText, extra = {}) =>
	JSON.stringify({ tool_name: "edit", tool_input: { path: "x", oldText, newText, ...extra } });
const protect = (file, ...rest) => ["protect", "--file", file, ...rest];
const ultron = (file, ...rest) => protect(file, "--preview", "ultron", ...rest);
const cfg = (value) => ({ ".loki/loki.json": JSON.stringify(value) });
const grant = (command, cwd = ".", inputs = ["a.txt"]) => ({ shell_commands: [{ command, cwd, inputs }] });

const workflow = [
	"name: ci",
	"on: pull_request",
	"jobs:",
	"  build:",
	"    steps:",
	"      - run: echo ${{ github.head_ref }}",
	"      - name: multi",
	"        run: |",
	"          echo \"${{ github.event.pull_request.title }}\"",
	"          echo ${{ github.sha }}",
	"        env:",
	"          BODY: ${{ github.event.issue.body }}",
	"      - run: echo ok",
	"        with: ${{ github.event.comment.body }}",
	"",
].join("\n");

const cases = [
	{ name: "plain-file-passes", files: { "a.txt": "x" }, args: protect("a.txt") },
	{ name: "new-file-passes", args: protect("src/new.txt") },
	{ name: "loki-dir-protected", args: protect(".loki/loki.json") },
	{ name: "nested-credo-protected", args: protect("apps/web/.credo.exs") },
	{ name: "workflow-loki-protected", args: protect("./.github/workflows/loki.yml") },
	{ name: "dotdot-normalized-protected", args: protect("src/../.ruff.toml") },
	{ name: "outside-root", args: protect("../elsewhere.txt") },
	{ name: "absolute-outside-root", args: protect("/etc/hostname") },
	{ name: "directory-target", files: { "src/a.txt": "" }, args: protect("src") },
	{ name: "symlink-into-protected", files: { "link": { symlink: ".loki/x" } }, args: protect("link") },
	{ name: "protected-extra-glob", files: cfg({ protected_extra: ["docs/*.md"] }), args: protect("docs/sub/a.md") },
	{ name: "protected-extra-subtree", files: cfg({ protected_extra: ["vendor/**"] }), args: protect("vendor") },
	{ name: "protected-extra-charset", files: cfg({ protected_extra: ["[!a]*.lock"] }), args: protect("b.lock") },
	{ name: "protected-extra-charset-miss", files: cfg({ protected_extra: ["[!a]*.lock"] }), args: protect("a.lock") },
	{ name: "allow-protected-env", env: { LOKI_ALLOW_PROTECTED: "1" }, args: protect(".ruff.toml") },
	{ name: "multiple-files-first-protected", args: [...protect(".ruff.toml"), "--file", "buf.yaml", "--file", ".ruff.toml"] },
	{ name: "json-blocked", args: [...protect(".loki/x"), "--format", "json"] },
	{ name: "json-passed", args: [...protect("a.txt"), "--format", "json"] },
	{ name: "json-root-flag-after-command", args: ["protect", "--format=json", "--file=a.txt"], rootAfter: true },
	{ name: "empty-path", args: protect("") },
	{ name: "empty-path-json", args: [...protect(""), "--format", "json"] },
	{ name: "template-config-valid", files: cfg({ approved_dependencies: null, rule_packs: ["core", "python", "typescript", "phoenix", "shell"], elixir_security: { sobelow: { block: "high", warn: "medium" } }, shell_commands: [], commands: { contract: [], property: [], differential: [], mutation: [] } }), args: protect("a.txt") },
	{ name: "config-not-object", files: cfg([1]), args: protect("a.txt") },
	{ name: "config-unknown-key", files: cfg({ zeta: 1, alpha: 2 }), args: protect("a.txt") },
	{ name: "config-protected-extra-empty", files: cfg({ protected_extra: [""] }), args: protect("a.txt") },
	{ name: "config-rule-pack-unknown", files: cfg({ rule_packs: ["core", "java"] }), args: protect("a.txt") },
	{ name: "config-elixir-security-option", files: cfg({ elixir_security: { credo: {} } }), args: protect("a.txt") },
	{ name: "config-sobelow-not-object", files: cfg({ elixir_security: { sobelow: [] } }), args: protect("a.txt") },
	{ name: "config-sobelow-option", files: cfg({ elixir_security: { sobelow: { fail: "x" } } }), args: protect("a.txt") },
	{ name: "config-sobelow-level", files: cfg({ elixir_security: { sobelow: { block: "max" } } }), args: protect("a.txt") },
	{ name: "config-sobelow-order", files: cfg({ elixir_security: { sobelow: { block: "low", warn: "high" } } }), args: protect("a.txt") },
	{ name: "config-sobelow-none-ok", files: cfg({ elixir_security: { sobelow: { block: "none", warn: "high" } } }), args: protect("a.txt") },
	{ name: "config-shell-not-list", files: cfg({ shell_commands: {} }), args: protect("a.txt") },
	{ name: "config-shell-keys", files: cfg({ shell_commands: [{ command: "make" }] }), args: protect("a.txt") },
	{ name: "config-shell-git", files: cfg(grant("git status")), args: protect("a.txt") },
	{ name: "config-shell-syntax", files: cfg(grant("make > out")), args: protect("a.txt") },
	{ name: "config-shell-empty", files: cfg(grant("   ")), args: protect("a.txt") },
	{ name: "config-shell-quote", files: cfg(grant("make 'x")), args: protect("a.txt") },
	{ name: "config-shell-path-exe", files: cfg(grant("./run")), args: protect("a.txt") },
	{ name: "config-shell-assign", files: cfg(grant("A=1 make")), args: protect("a.txt") },
	{ name: "config-shell-inputs-empty", files: cfg(grant("make", ".", [])), args: protect("a.txt") },
	{ name: "config-shell-path-dotdot", files: cfg(grant("make", "a/../b")), args: protect("a.txt") },
	{ name: "config-shell-path-dot-prefix", files: cfg(grant("make", "./a")), args: protect("a.txt") },
	{ name: "config-shell-path-absolute", files: cfg(grant("make", "/tmp")), args: protect("a.txt") },
	{ name: "config-shell-input-dot", files: cfg(grant("make", ".", ["."])), args: protect("a.txt") },
	{ name: "config-shell-duplicate", files: cfg({ shell_commands: [grant("make").shell_commands[0], grant("make", ".", ["b"]).shell_commands[0]] }), args: protect("a.txt") },
	{ name: "config-shell-valid", files: cfg(grant("make \"check all\"", "src/app", ["src/app/Makefile"])), args: protect("a.txt") },
	{ name: "config-slop-shape", files: cfg({ slop: { size: 1 } }), args: protect("a.txt") },
	{ name: "config-slop-block", files: cfg({ slop: { block: 1 } }), args: protect("a.txt") },
	{ name: "config-slop-range", files: cfg({ slop: { max_index: 101 } }), args: protect("a.txt") },
	{ name: "config-slop-float", files: cfg({ slop: { max_index_increase: 5.5 } }), args: protect("a.txt") },
	{ name: "config-slop-bool", files: cfg({ slop: { max_index: true } }), args: protect("a.txt") },
	{ name: "config-typescript-check", files: cfg({ typescript_check: "yes" }), args: protect("a.txt") },
	{ name: "config-approved-deps", files: cfg({ approved_dependencies: ["ok", ""] }), args: protect("a.txt") },
	{ name: "config-private-packages", files: cfg({ private_packages: "x" }), args: protect("a.txt") },
	{ name: "config-languages-unknown", files: cfg({ languages: { java: true } }), args: protect("a.txt") },
	{ name: "config-languages-value", files: cfg({ languages: { python: 1 } }), args: protect("a.txt") },
	{ name: "config-commands-object", files: cfg({ commands: [] }), args: protect("a.txt") },
	{ name: "config-commands-group", files: cfg({ commands: { lint: [] } }), args: protect("a.txt") },
	{ name: "config-commands-shape", files: cfg({ commands: { mutation: [["mutmut", ""]] } }), args: protect("a.txt") },
	{ name: "config-error-json", files: cfg({ zeta: 1 }), args: [...protect("a.txt"), "--format", "json"] },
	{ name: "preview-secret-new-file", args: ultron("notes.txt"), stdin: write("write", "key = '@@AWS@@'\n") },
	{ name: "preview-secret-json", args: [...ultron("notes.txt"), "--format", "json"], stdin: write("write", "a\nkey = '@@AWS@@'\n") },
	{ name: "preview-secret-existing-not-new", files: { "notes.txt": "key = '@@AWS@@'\n" }, args: ultron("notes.txt"), stdin: write("write", "head\nkey = '@@AWS@@'\n") },
	{ name: "preview-secret-duplicated", files: { "notes.txt": "key = '@@AWS@@'\n" }, args: ultron("notes.txt"), stdin: write("write", "key = '@@AWS@@'\nkey = '@@AWS@@'\nkey = '@@AWS@@'\n") },
	{ name: "preview-secret-kinds", args: ultron("all.cfg"), stdin: write("write", ["@@AWS2@@", "@@GH@@", "@@PEM@@", "@@SLACK@@", "@@STRIPE@@", "@@NPM@@", "@@GOOGLE@@", "@@MODEL@@", ""].join("\n")) },
	{ name: "preview-secret-placeholder-and-low-entropy", args: ultron("a.env"), stdin: write("write", "A=@@FAKE@@\nB=@@LOW@@\n") },
	{ name: "preview-secret-entropy-between-2-and-3", args: ultron("m.env"), stdin: write("write", "M=@@MID@@\n") },
	{ name: "preview-secret-strip-unit-separator", files: { "s.txt": "k @@AWS@@\u001f\n" }, args: ultron("s.txt"), stdin: write("write", "k @@AWS@@\n") },
	{ name: "config-sobelow-equal-levels", files: cfg({ elixir_security: { sobelow: { block: "medium", warn: "medium" } } }), args: protect("a.txt") },
	{ name: "preview-secret-unicode-crlf", args: ultron("u.md"), stdin: write("write", "héllo wörld ✓\r\nsecond third @@GH@@\r\n") },
	{ name: "preview-conflict-markers", args: ultron("m.txt"), stdin: write("write", "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n") },
	{ name: "preview-conflict-half", args: ultron("m.txt"), stdin: write("write", "<<<<<<< ours\na\n") },
	{ name: "preview-workflow-injection", args: ultron(".github/workflows/ci.yml"), stdin: write("write", workflow) },
	{ name: "preview-action-injection", args: ultron(".github/actions/setup/action.yaml"), stdin: write("write", "runs:\n  steps:\n    - run: echo ${{ github.event.release.name }}\n") },
	{ name: "preview-workflow-path-only", args: ultron("docs/ci.yml"), stdin: write("write", workflow) },
	{ name: "preview-omp-write", args: protect("o.txt", "--preview", "omp"), stdin: write("write", "@@STRIPE@@\n") },
	{ name: "preview-pi-edit", files: { "e.txt": "a\nTOKEN\nb\n" }, args: protect("e.txt", "--preview", "pi"), stdin: piEdit("TOKEN", "@@NPM@@") },
	{ name: "preview-pi-edit-ambiguous", files: { "e.txt": "T\nT\n" }, args: protect("e.txt", "--preview", "pi"), stdin: piEdit("T", "@@NPM@@") },
	{ name: "preview-pi-edit-all", files: { "e.txt": "T\nT\n" }, args: protect("e.txt", "--preview", "pi"), stdin: piEdit("T", "@@NPM@@", { replace_all: true }) },
	{ name: "preview-pi-edit-mode", files: { "e.txt": "T\n" }, args: protect("e.txt", "--preview", "pi"), stdin: piEdit("T", "x", { replace_all: "yes" }) },
	{ name: "preview-pi-edit-missing-file", args: protect("e.txt", "--preview", "pi"), stdin: piEdit("T", "x") },
	{ name: "preview-pi-edit-missing-file-js", args: protect("e.js", "--preview", "pi"), stdin: piEdit("T", "x") },
	{ name: "preview-unknown-tool-txt", args: ultron("a.txt"), stdin: JSON.stringify({ tool_name: "edit", tool_input: {} }) },
	{ name: "preview-unknown-tool-ts", args: ultron("a.ts"), stdin: JSON.stringify({ tool_name: "edit", tool_input: {} }) },
	{ name: "preview-unknown-tool-ts-strict", env: { LOKI_STRICT: "1" }, args: ultron("a.ts"), stdin: JSON.stringify({ tool_name: "edit", tool_input: {} }) },
	{ name: "preview-unknown-tool-ts-json", args: [...ultron("a.ts"), "--format", "json"], stdin: JSON.stringify({ tool_name: "edit", tool_input: {} }) },
	{ name: "preview-bad-envelope-txt", args: ultron("a.txt"), stdin: JSON.stringify({ tool_name: "write" }) },
	{ name: "preview-bad-envelope-mjs", args: ultron("a.mjs"), stdin: JSON.stringify([]) },
	{ name: "preview-content-not-text-tsx", args: ultron("a.tsx"), stdin: JSON.stringify({ tool_name: "write", tool_input: { content: 1 } }) },
	{ name: "preview-path-only-write", args: ultron("a.txt"), stdin: JSON.stringify({ tool_name: "write", tool_input: { path: "a.txt" } }) },
	{ name: "preview-symlinked-path", files: { "real.txt": "", "link.txt": { symlink: "real.txt" } }, args: ultron("link.txt"), stdin: write("write", "@@AWS@@") },
	{ name: "preview-protected-skips-preview", args: ultron(".ruff.toml"), stdin: write("write", "@@AWS@@") },
	{ name: "context-default", args: ["context"] },
	{ name: "context-ultron", args: ["context", "--host", "ultron", "--event", "UserPromptSubmit"] },
	{ name: "context-ultron-no-packs", files: cfg({ rule_packs: ["shell"] }), args: ["context", "--host", "ultron"] },
	{ name: "context-phoenix-none", files: cfg({ rule_packs: ["core", "phoenix"], elixir_security: { sobelow: { block: "none", warn: "none" } } }), args: ["context"] },
	{ name: "context-phoenix-levels", files: cfg({ rule_packs: ["phoenix", "phoenix"], elixir_security: { sobelow: { block: "medium", warn: "low" } } }), args: ["context"] },
	{ name: "context-invalid-config", files: cfg({ rule_packs: "core" }), args: ["context"] },
];

function run(testCase) {
	const root = mkdtempSync(join(tmpdir(), "loki-fixture-"));
	try {
		for (const [path, content] of Object.entries(testCase.files ?? {})) {
			mkdirSync(dirname(join(root, path)), { recursive: true });
			if (typeof content === "string") writeFileSync(join(root, path), expand(content));
			else symlinkSync(content.symlink, join(root, path));
		}
		const argv = testCase.rootAfter ? [...testCase.args, "--root", root] : ["--root", root, ...testCase.args];
		const env = { PATH: process.env.PATH, HOME: root, ...(testCase.env ?? {}) };
		const result = spawnSync(python, [engine, ...argv], {
			cwd: root,
			env,
			input: expand(testCase.stdin ?? ""),
			encoding: "utf8",
		});
		const scrub = (text) => text.split(root).join("<ROOT>");
		return { status: result.status, stdout: scrub(result.stdout), stderr: scrub(result.stderr) };
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
}

const recorded = cases.map((testCase) => ({ ...testCase, expected: run(testCase) }));
writeFileSync(
	join(here, "protect-corpus.json"),
	`${JSON.stringify({ engine: "packages/coding-agent/src/ultron/loki-engine/loki.py", tokens, cases: recorded }, null, "\t")}\n`,
);
console.log(`recorded ${recorded.length} cases`);
