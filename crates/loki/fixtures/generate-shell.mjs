// Records loki.py's `shell`, `shell-writes` and `explain` behavior for the Rust differential tests.
// Run: node crates/loki/fixtures/generate-shell.mjs  (writes shell-corpus.json next to this file)
// Python: $ULTRON_PYTHON, else /usr/bin/python3. Deterministic: run twice and compare sha256.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const engine = join(here, "../../../packages/coding-agent/src/ultron/loki-engine/loki.py");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";

const cfg = (value) => JSON.stringify(value);
const grant = (command, cwd = ".", inputs = ["Makefile"]) => cfg({ shell_commands: [{ command, cwd, inputs }] });
const bash = (command, extra = {}) => ({ tool_name: "Bash", cwd: "<ROOT>", tool_input: { command }, ...extra });
const shell = (command, extra) => ({ args: ["shell", "--harness", "claude"], stdin: bash(command, extra) });
const writes = (command, cwd = "<ROOT>") => ({ args: ["shell-writes"], stdin: { command, cwd } });
const repo = { ".loki/loki.json": grant("make check"), Makefile: "check:\n\ttrue\n" };

const cases = [
	// shell: default-deny guard
	{ name: "shell-git-status", ...shell("git status") },
	{ name: "shell-git-diff-output", ...shell("git diff --output=x") },
	{ name: "shell-git-log-exec", ...shell("git log --exec=x") },
	{ name: "shell-pwd", ...shell("pwd") },
	{ name: "shell-pwd-args", ...shell("pwd -P") },
	{ name: "shell-echo", ...shell("echo hi there") },
	{ name: "shell-printf", ...shell("printf '%s' x") },
	{ name: "shell-git-reset-hard", ...shell("git reset --hard HEAD~1") },
	{ name: "shell-git-reset-keep", ...shell("git reset --keep HEAD") },
	{ name: "shell-git-reset-soft", ...shell("git reset --soft HEAD") },
	{ name: "shell-git-clean-fdx", ...shell("git clean -fdx") },
	{ name: "shell-git-clean-dry", ...shell("git clean -n") },
	{ name: "shell-git-clean-force", ...shell("git clean --force") },
	{ name: "shell-git-push-force", ...shell("git push --force origin main") },
	{ name: "shell-git-push-plus", ...shell("git push origin +main") },
	{ name: "shell-git-push-delete-colon", ...shell("git push origin :old") },
	{ name: "shell-git-push-plain", ...shell("git push origin main") },
	{ name: "shell-git-branch-D", ...shell("git branch -D old") },
	{ name: "shell-git-branch-delete-force", ...shell("git branch --delete --force old") },
	{ name: "shell-git-branch-delete", ...shell("git branch --delete old") },
	{ name: "shell-git-stash-drop", ...shell("git stash drop") },
	{ name: "shell-git-checkout-dot", ...shell("git checkout .") },
	{ name: "shell-git-checkout-branch", ...shell("git checkout main") },
	{ name: "shell-git-restore", ...shell("git restore a.txt") },
	{ name: "shell-git-rebase-long", ...shell("git rebase -i HEAD~3 --autosquash x") },
	{ name: "shell-git-commit", ...shell("git commit -m x") },
	{ name: "shell-pipe", ...shell("cat a | grep b") },
	{ name: "shell-subst", ...shell("echo $(id)") },
	{ name: "shell-glob", ...shell("ls *.txt") },
	{ name: "shell-control-char", ...shell("echo a\tb") },
	{ name: "shell-too-long", ...shell(`echo ${"a".repeat(4100)}`) },
	{ name: "shell-empty", ...shell("   ") },
	{ name: "shell-not-string", ...shell(5) },
	{ name: "shell-unclosed-quote", ...shell("echo 'a") },
	{ name: "shell-path-exe", ...shell("./run.sh") },
	{ name: "shell-assign-exe", ...shell("A=1 make") },
	// shell: redirects
	{ name: "shell-redirect-notes", ...shell("echo hi > notes.md") },
	{ name: "shell-redirect-append-log", ...shell("printf x >> build.log") },
	{ name: "shell-redirect-source", ...shell("echo x > main.py") },
	{ name: "shell-redirect-hidden", ...shell("echo x > .env.txt") },
	{ name: "shell-redirect-hidden-dir", ...shell("echo x > .cache/a.txt") },
	{ name: "shell-redirect-outside", ...shell("echo x > ../out.txt") },
	{ name: "shell-redirect-absolute-outside", ...shell("echo x > /tmp/loki-out.txt") },
	{ name: "shell-redirect-protected", ...shell("echo x > .loki/notes.md") },
	{ name: "shell-redirect-dir", files: { "d.md/keep": "" }, ...shell("echo x > d.md") },
	{ name: "shell-redirect-symlink", files: { "real.md": "", "link.md": { symlink: "real.md" } }, ...shell("echo x > link.md") },
	{ name: "shell-redirect-cat", ...shell("cat a > b.txt") },
	{ name: "shell-redirect-two-ops", ...shell("echo a > b.txt; echo c") },
	{ name: "shell-redirect-quoted-target", ...shell("echo hi > 'my notes.txt'") },
	{ name: "shell-redirect-comment", ...shell("echo hi > n.txt # done") },
	{ name: "shell-redirect-subdir-cwd", files: { "sub/keep": "" }, args: ["shell", "--harness", "claude"], stdin: bash("echo x > ../top.md", { cwd: "<ROOT>/sub" }) },
	{ name: "shell-redirect-invalid-config", files: { ".loki/loki.json": cfg({ zeta: 1 }) }, ...shell("echo x > a.md") },
	// shell: envelope and cwd
	{ name: "shell-factory-name", args: ["shell", "--harness", "factory"], stdin: { ...bash("pwd"), tool_name: "Execute" } },
	{ name: "shell-pi-name", args: ["shell", "--harness", "pi"], stdin: { ...bash("pwd"), tool_name: "bash" } },
	{ name: "shell-wrong-name", args: ["shell", "--harness", "factory"], stdin: bash("pwd") },
	{ name: "shell-extra-field", ...shell("pwd"), stdin: { ...bash("pwd"), tool_input: { command: "pwd", env: {} } } },
	{ name: "shell-meta-fields", ...shell("pwd"), stdin: { ...bash("pwd"), tool_input: { command: "pwd", description: "d", timeout: 5 } } },
	{ name: "shell-cwd-missing", ...shell("pwd"), stdin: { tool_name: "Bash", tool_input: { command: "pwd" } } },
	{ name: "shell-cwd-relative", ...shell("pwd"), stdin: bash("pwd", { cwd: "sub" }) },
	{ name: "shell-cwd-outside", ...shell("pwd"), stdin: bash("pwd", { cwd: "/" }) },
	{ name: "shell-cwd-dotdot", files: { "sub/keep": "" }, ...shell("pwd"), stdin: bash("pwd", { cwd: "<ROOT>/sub/.." }) },
	{ name: "shell-cwd-dot", files: { "sub/keep": "" }, ...shell("pwd"), stdin: bash("pwd", { cwd: "<ROOT>/./sub/" }) },
	{ name: "shell-cwd-symlink", files: { "real/keep": "", link: { symlink: "real" } }, ...shell("pwd"), stdin: bash("pwd", { cwd: "<ROOT>/link" }) },
	{ name: "shell-cwd-not-dir", files: { "f.txt": "" }, ...shell("pwd"), stdin: bash("pwd", { cwd: "<ROOT>/f.txt" }) },
	{ name: "shell-tool-input-not-object", ...shell("pwd"), stdin: { tool_name: "Bash", cwd: "<ROOT>", tool_input: "pwd" } },
	{ name: "shell-envelope-list", ...shell("pwd"), stdin: [] },
	// shell: committed grants
	{ name: "shell-grant-no-repo", ...shell("make check") },
	{ name: "shell-grant-allowed", files: repo, git: true, ...shell("make check") },
	{ name: "shell-grant-other-command", files: repo, git: true, ...shell("make all") },
	{ name: "shell-grant-wrong-cwd", files: { ...repo, "sub/keep": "" }, git: true, ...shell("make check"), stdin: bash("make check", { cwd: "<ROOT>/sub" }) },
	{ name: "shell-grant-subdir-cwd", files: { ".loki/loki.json": grant("make", "sub", ["sub/Makefile"]), "sub/Makefile": "all:\n" }, git: true, args: ["shell", "--harness", "claude"], stdin: bash("make", { cwd: "<ROOT>/sub" }) },
	{ name: "shell-grant-input-changed", files: repo, git: true, after: { Makefile: "check:\n\trm -rf /\n" }, ...shell("make check") },
	{ name: "shell-grant-policy-changed", files: repo, git: true, after: { ".loki/loki.json": grant("make check", ".", ["Makefile", "x"]) }, ...shell("make check") },
	{ name: "shell-grant-input-deleted", files: repo, git: true, after: { Makefile: null }, ...shell("make check") },
	{ name: "shell-grant-input-mode", files: repo, git: true, chmod: { Makefile: 0o755 }, ...shell("make check") },
	{ name: "shell-grant-uncommitted-input", files: { ".loki/loki.json": grant("make check", ".", ["Makefile", "extra.mk"]), Makefile: "" }, git: true, ...shell("make check") },
	{ name: "shell-grant-uncommitted-policy", files: { Makefile: "" }, git: true, after: { ".loki/loki.json": grant("make check") }, ...shell("make check") },
	{ name: "shell-grant-input-is-dir", files: { ".loki/loki.json": grant("make check", ".", ["src"]), "src/a": "" }, git: true, ...shell("make check") },
	{ name: "shell-grant-quoted-command", files: { ".loki/loki.json": grant("make \"check all\""), Makefile: "" }, git: true, ...shell("make \"check all\"") },
	{ name: "shell-grant-quoted-differs", files: { ".loki/loki.json": grant("make \"check all\""), Makefile: "" }, git: true, ...shell("make 'check all'") },
	{ name: "shell-grant-root-not-toplevel", files: { "inner/keep": "" }, git: true, rootSub: "inner", ...shell("make check") },
	// shell-writes
	{ name: "writes-plain", ...writes("ls -la") },
	{ name: "writes-redirect-source", ...writes("echo x > src/main.rs") },
	{ name: "writes-redirect-notes", ...writes("echo x > notes.md") },
	{ name: "writes-redirect-log-hidden", ...writes("echo x > .cache/run.log") },
	{ name: "writes-redirect-dev-null", ...writes("make 2> /dev/null") },
	{ name: "writes-fd-dup", ...writes("make 2>&1 | cat") },
	{ name: "writes-amp-redirect", ...writes("make &> build.ts") },
	{ name: "writes-clobber", ...writes("echo x >| a.py") },
	{ name: "writes-semicolon-redirect", ...writes("true;> a.py") },
	{ name: "writes-tee", ...writes("make | tee -a out.log src/x.ts") },
	{ name: "writes-sed-inplace", ...writes("sed -i 's/a/b/' app.py") },
	{ name: "writes-sed-script", ...writes("sed -i.bak -e 's/a/b/' app.py notes.md") },
	{ name: "writes-sed-not-inplace", ...writes("sed 's/a/b/' app.py") },
	{ name: "writes-perl-inplace", ...writes("perl -pi -e 's/a/b/' lib.pm") },
	{ name: "writes-cp", ...writes("cp a.txt b.py") },
	{ name: "writes-mv-dir-slash", ...writes("mv a.py b.py src/") },
	{ name: "writes-cp-existing-dir", files: { "lib/keep": "" }, ...writes("cp x/a.py lib") },
	{ name: "writes-install-t", ...writes("install -t bin tool.sh") },
	{ name: "writes-dd", ...writes("dd if=/dev/zero of=disk.img bs=1") },
	{ name: "writes-env-prefix", ...writes("sudo env A=1 tee config.yaml") },
	{ name: "writes-heredoc", ...writes("cat > gen.py <<'EOF'\nprint(1) > x\nEOF\necho done") },
	{ name: "writes-heredoc-body-only", ...writes("cat <<EOF\n> a.py\nEOF") },
	{ name: "writes-outside-root", ...writes("echo x > /tmp/elsewhere.py") },
	{ name: "writes-variable-target", ...writes("echo x > $OUT") },
	{ name: "writes-glob-target", ...writes("cp a *.py") },
	{ name: "writes-protected", ...writes("echo x > .ruff.toml") },
	{ name: "writes-protected-loki", ...writes("tee .loki/loki.json") },
	{ name: "writes-gitignored", files: { ".gitignore": "dist/\n" }, git: true, ...writes("echo x > dist/out.js") },
	{ name: "writes-not-ignored-in-repo", files: { ".gitignore": "dist/\n" }, git: true, ...writes("echo x > src/out.js") },
	{ name: "writes-cwd-subdir", files: { "sub/keep": "" }, ...writes("echo x > a.py", "<ROOT>/sub") },
	{ name: "writes-cwd-relative", files: { "sub/keep": "" }, ...writes("echo x > a.py", "sub") },
	{ name: "writes-unparseable", ...writes("echo 'x > a.py") },
	{ name: "writes-empty", ...writes("  ") },
	{ name: "writes-not-string", args: ["shell-writes"], stdin: { command: ["x"] } },
	{ name: "writes-not-object", args: ["shell-writes"], stdin: "x" },
	{ name: "writes-digit-redirect", ...writes("make 1> out.ts") },
	{ name: "writes-uppercase-suffix", ...writes("echo x > README.MD") },
	{ name: "writes-patch", ...writes("git diff > fix.patch") },
	{ name: "writes-invalid-config", files: { ".loki/loki.json": cfg({ rule_packs: "core" }) }, ...writes("echo x > a.py") },
	{ name: "writes-harness-flag", args: ["shell-writes", "--harness", "ultron"], stdin: { command: "echo x > a.py" } },
	// explain
	{ name: "explain-empty", args: ["explain"] },
	{ name: "explain-events", events: ['{"time": 2, "operation": "hook", "b": [1, {"z": 1, "a": 2}]}', '{"é": "ü", "A": null}'], args: ["explain"] },
	{ name: "explain-limit", events: ['{"n": 1}', '{"n": 2}', '{"n": 3}'], args: ["explain", "--limit", "2"] },
	{ name: "explain-limit-zero", args: ["explain", "--limit", "0"] },
	{ name: "explain-limit-max", events: ['{"n": 1}'], args: ["explain", "--limit", "1000"] },
	{ name: "explain-limit-high", args: ["explain", "--limit=1001"] },
	// argument errors
	{ name: "args-shell-no-harness", args: ["shell"] },
	{ name: "args-shell-bad-harness", args: ["shell", "--harness", "codex"] },
	{ name: "args-writes-bad-harness", args: ["shell-writes", "--harness", "pi"] },
	{ name: "args-explain-bad-limit", args: ["explain", "--limit", "x"] },
	{ name: "args-shell-unknown", args: ["shell", "--harness", "claude", "--bogus"] },
];

const git = (cwd, ...args) => {
	const result = spawnSync("git", ["-c", "user.name=loki", "-c", "user.email=loki@example.invalid", "-c", "commit.gpgsign=false", ...args], {
		cwd,
		env: { PATH: process.env.PATH, HOME: cwd, GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z", GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z" },
		encoding: "utf8",
	});
	if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
};

function place(root, files) {
	for (const [path, content] of Object.entries(files ?? {})) {
		const target = join(root, path);
		if (content === null) {
			rmSync(target);
			continue;
		}
		mkdirSync(dirname(target), { recursive: true });
		if (typeof content === "string") writeFileSync(target, content);
		else symlinkSync(content.symlink, target);
	}
}

function run(testCase) {
	const top = realpathSync(mkdtempSync(join(tmpdir(), "loki-shell-")));
	try {
		place(top, testCase.files);
		if (testCase.git) {
			git(top, "init", "-q", "-b", "main");
			git(top, "add", "-A");
			git(top, "commit", "-q", "-m", "base");
		}
		place(top, testCase.after);
		for (const [path, mode] of Object.entries(testCase.chmod ?? {})) chmodSync(join(top, path), mode);
		const root = testCase.rootSub ? join(top, testCase.rootSub) : top;
		if (testCase.events) {
			const identity = createHash("sha256").update(root).digest("hex");
			const dir = join(top, ".local/state/loki", identity);
			mkdirSync(dir, { recursive: true });
			writeFileSync(join(dir, "events.jsonl"), testCase.events.map((line) => `${line}\n`).join(""));
		}
		const stdin = testCase.stdin === undefined ? "" : JSON.stringify(testCase.stdin).split("<ROOT>").join(root);
		const result = spawnSync(python, [engine, "--root", root, ...testCase.args], {
			cwd: root,
			env: { PATH: process.env.PATH, HOME: top },
			input: stdin,
			encoding: "utf8",
		});
		const scrub = (text) => text.split(top).join("<ROOT>");
		return { status: result.status, stdout: scrub(result.stdout), stderr: scrub(result.stderr) };
	} finally {
		rmSync(top, { recursive: true, force: true });
	}
}

const recorded = cases.map((testCase) => ({ ...testCase, expected: run(testCase) }));
writeFileSync(
	join(here, "shell-corpus.json"),
	`${JSON.stringify({ engine: "packages/coding-agent/src/ultron/loki-engine/loki.py", cases: recorded }, null, "\t")}\n`,
);
console.log(`recorded ${recorded.length} cases`);
