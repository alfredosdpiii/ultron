import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
	globToRegExp,
	main,
	parseAllowlist,
	scanDiff,
	scanDirectory,
	scanTarball,
	scanTree,
} from "./secret-scan.mjs";

// Fake values, built from parts so this file holds none whole.
const BODY = "FAKE0a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0u";
const GITHUB = ["g", "hp_", BODY.slice(0, 36)].join("");
const OPENAI = ["s", "k-proj-", BODY].join("");
const HOME = ["/ho", "me/jdoe-fake/"].join("");
const PEM = [
	["-----BEGIN ", "PRIV", "ATE KEY-----"].join(""),
	`${BODY}${BODY}`,
	BODY,
	["-----END ", "PRIV", "ATE KEY-----"].join(""),
].join("\n");

function tempDir(prefix) {
	return mkdtempSync(join(tmpdir(), prefix));
}

function write(root, path, text) {
	mkdirSync(join(root, path, ".."), { recursive: true });
	writeFileSync(join(root, path), text);
}

function git(cwd, ...args) {
	return execFileSync("git", args, { cwd, encoding: "utf8", env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null" } });
}

function initRepo(dir) {
	git(dir, "init", "-q");
	git(dir, "config", "user.email", "scan@example.com");
	git(dir, "config", "user.name", "scan");
	git(dir, "config", "commit.gpgsign", "false");
}

test("a directory: findings are redacted, binaries and node_modules skipped, the allowlist excuses by path and rule", () => {
	const dir = tempDir("secret-scan-dir-");
	try {
		write(dir, "src/config.py", `DEBUG = True\nTOKEN = "${GITHUB}"\n`);
		write(dir, "src/clean.ts", 'export const token = process.env.GITHUB_TOKEN ?? "";\n');
		write(dir, "docs/setup.md", `Run it from ${HOME}project.\n`);
		write(dir, "test/fixtures/fake.env", `OPENAI=${OPENAI}\nPATH_HINT=${HOME}x\n`);
		write(dir, "node_modules/dep/index.js", `const k = "${OPENAI}";\n`);
		writeFileSync(join(dir, "blob.bin"), Buffer.concat([Buffer.from([0, 1, 2]), Buffer.from(GITHUB)]));

		const all = scanDirectory(dir);
		// Files are walked in name order.
		assert.deepEqual(
			all.findings.map((f) => [f.path, f.line, f.kind]),
			[
				["docs/setup.md", 1, "home_path"],
				["src/config.py", 2, "github_token"],
				["test/fixtures/fake.env", 1, "openai_key"],
				["test/fixtures/fake.env", 2, "home_path"],
			],
		);
		assert.equal(all.skipped, 1);
		for (const finding of all.findings) {
			assert.ok(!finding.preview.includes(GITHUB) && !finding.preview.includes(OPENAI), finding.preview);
		}
		assert.equal(all.findings.find((f) => f.kind === "github_token").column, 10);

		// The fixture's fake key is excused; its path hint is not, because the entry names only openai_key.
		const allowlist = parseAllowlist("# fixtures\ntest/fixtures/** openai_key\n");
		const allowed = scanDirectory(dir, { allowlist });
		assert.deepEqual(
			allowed.findings.map((f) => `${f.path}:${f.kind}`).sort(),
			["docs/setup.md:home_path", "src/config.py:github_token", "test/fixtures/fake.env:home_path"],
		);

		// The CLI prints redacted findings to stderr and exits 1; a clean directory exits 0.
		const errors = [];
		assert.equal(main(["--dir", dir, "--no-allow"], { log: () => {}, error: (line) => errors.push(line) }), 1);
		const printed = errors.join("\n");
		assert.match(printed, /src\/config\.py:2:10 {2}github_token {2}ghp_…\(40 chars\)/);
		assert.ok(!printed.includes(GITHUB));
		const clean = tempDir("secret-scan-clean-");
		write(clean, "a.txt", "nothing here\n");
		const logs = [];
		assert.equal(main(["--dir", clean], { log: (line) => logs.push(line), error: () => {} }), 0);
		assert.match(logs.join("\n"), /clean \(1 files/);
		rmSync(clean, { recursive: true, force: true });
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test("a diff range: only added lines, with the new file's line numbers, a key block seen whole", () => {
	const dir = tempDir("secret-scan-diff-");
	try {
		initRepo(dir);
		write(dir, "app.py", `OLD = "${OPENAI}"\nx = 1\n`);
		git(dir, "add", ".");
		git(dir, "commit", "-qm", "base");
		write(dir, "app.py", `OLD = "${OPENAI}"\nx = 1\ny = 2\nTOKEN = "${GITHUB}"\n`);
		write(dir, "keys/id.pem", `${PEM}\n`);
		git(dir, "add", ".");
		git(dir, "commit", "-qm", "change");
		const result = scanDiff("HEAD~1..HEAD", { cwd: dir });
		assert.deepEqual(
			result.findings.map((f) => [f.path, f.line, f.kind]),
			[
				["app.py", 4, "github_token"],
				["keys/id.pem", 1, "private_key"],
			],
		);
		// The key already in the base is not part of the change.
		assert.ok(!result.findings.some((f) => f.kind === "openai_key"));
		// The whole tracked tree does contain it.
		assert.ok(scanTree({ cwd: dir }).findings.some((f) => f.path === "app.py" && f.line === 1));
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test("a tarball: unpacked and scanned whole, node_modules included; the allowlist matches under package/", () => {
	const dir = tempDir("secret-scan-tgz-");
	try {
		write(dir, "stage/package/package.json", '{"name":"x"}\n');
		write(dir, "stage/package/node_modules/@scope/dep/index.js", `module.exports = "${OPENAI}";\n`);
		write(dir, "stage/package/examples/demo.sh", `cd ${HOME}work\n`);
		execFileSync("tar", ["-czf", join(dir, "x.tgz"), "-C", join(dir, "stage"), "package"]);
		const result = scanTarball(join(dir, "x.tgz"));
		assert.deepEqual(
			result.findings.map((f) => `${f.path}:${f.kind}`).sort(),
			["package/examples/demo.sh:home_path", "package/node_modules/@scope/dep/index.js:openai_key"],
		);
		const allowlist = parseAllowlist("**/examples/demo.sh home_path\n");
		assert.equal(scanTarball(join(dir, "x.tgz"), { allowlist }).findings.length, 1);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test("globs and usage errors", () => {
	assert.ok(globToRegExp("test/fixtures/*.jsonl").test("test/fixtures/a.jsonl"));
	assert.ok(!globToRegExp("test/fixtures/*.jsonl").test("test/fixtures/deep/a.jsonl"));
	assert.ok(globToRegExp("**/examples/a.ts").test("examples/a.ts"));
	assert.ok(globToRegExp("**/examples/a.ts").test("package/examples/a.ts"));
	assert.ok(globToRegExp("a/**").test("a/b/c.txt"));
	assert.ok(!globToRegExp("a.b").test("axb"));
	assert.equal(main(["--diff"], { log: () => {}, error: () => {} }), 2);
	assert.equal(main(["--bogus"], { log: () => {}, error: () => {} }), 2);
});

test("the repository's own tree is clean", () => {
	const errors = [];
	assert.equal(main([], { log: () => {}, error: (line) => errors.push(line) }), 0, errors.join("\n"));
});
